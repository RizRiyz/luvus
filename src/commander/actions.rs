//! Typed Commander slash actions. These reuse App operations directly; slash
//! text is never passed to a terminal or interpreted as a CLI command.

use super::{parse_scoped_target, target_lookup};
use crate::app::{AgentForkError, App, OrchFormKind};
use crate::ids::PaneId;
use crate::layout::Axis;

pub(crate) struct SlashActionSpec {
    pub name: &'static str,
    pub usage: &'static str,
    pub needs_target: bool,
}

pub(crate) const SLASH_ACTIONS: [SlashActionSpec; 9] = [
    SlashActionSpec {
        name: "/focus",
        usage: "@pane | @tab:name | @workspace:name",
        needs_target: true,
    },
    SlashActionSpec {
        name: "/read",
        usage: "@pane",
        needs_target: true,
    },
    SlashActionSpec {
        name: "/fork",
        usage: "@agent-pane",
        needs_target: true,
    },
    SlashActionSpec {
        name: "/split",
        usage: "@pane right | below",
        needs_target: true,
    },
    SlashActionSpec {
        name: "/task",
        usage: "",
        needs_target: false,
    },
    SlashActionSpec {
        name: "/automation",
        usage: "",
        needs_target: false,
    },
    SlashActionSpec {
        name: "/mission",
        usage: "",
        needs_target: false,
    },
    SlashActionSpec {
        name: "/diff",
        usage: "",
        needs_target: false,
    },
    SlashActionSpec {
        name: "/files",
        usage: "",
        needs_target: false,
    },
];

/// Only show the picker while editing the first word. An exact action keeps
/// the full catalog visible so repeated Tab can cycle to another action.
pub(crate) fn slash_suggestions(
    draft: &str,
    cursor: usize,
) -> Option<(Vec<&'static SlashActionSpec>, usize)> {
    if !draft.starts_with('/') {
        return None;
    }
    let end = draft.find(char::is_whitespace).unwrap_or(draft.len());
    if cursor > end {
        return None;
    }
    let token = &draft[..end];
    let exact = SLASH_ACTIONS.iter().position(|spec| spec.name == token);
    let matches: Vec<_> = SLASH_ACTIONS
        .iter()
        .filter(|spec| exact.is_some() || spec.name.starts_with(token))
        .collect();
    let selected = exact.unwrap_or(0);
    Some((matches, selected))
}

impl super::Commander {
    pub(crate) fn slash_menu(&self) -> Option<(Vec<&'static SlashActionSpec>, usize)> {
        let (matches, default) = slash_suggestions(&self.draft, self.cursor)?;
        let selected = self
            .slash_selection
            .unwrap_or(default)
            .min(matches.len().saturating_sub(1));
        Some((matches, selected))
    }
}
const READ_LINES: usize = 80;
const READ_LINE_CHARS: usize = 256;
const READ_HEIGHT: u16 = 12;

#[derive(Debug)]
pub(crate) enum SlashAction {
    Focus(FocusTarget),
    Read(ActionPane),
    Fork(ActionPane),
    Split(ActionPane, Axis),
    Task,
    Automation,
    Mission,
    Diff,
    Files,
}

#[derive(Debug)]
pub(crate) enum FocusTarget {
    Pane(PaneId),
    Tab {
        workspace: usize,
        tab: usize,
        workspace_id: String,
        tab_id: String,
    },
    Workspace {
        index: usize,
        workspace_id: String,
    },
}

#[derive(Debug)]
pub(crate) struct ActionPane {
    id: PaneId,
    terminal_id: String,
}

impl App {
    pub(crate) fn commander_parse_slash_action(
        &self,
        draft: &str,
    ) -> Option<Result<SlashAction, String>> {
        let trimmed = draft.trim();
        if !trimmed.starts_with('/') {
            return None;
        }
        Some((|| {
            let mut words = trimmed.split_whitespace();
            let action = words.next().unwrap();
            let target = words.next();
            match action {
                "/focus" => {
                    let pane = self.commander_focus_target(
                        target.ok_or("Use /focus @pane, @tab:name, or @workspace:name")?,
                    )?;
                    if words.next().is_some() {
                        return Err("/focus takes one exact target".into());
                    }
                    Ok(SlashAction::Focus(pane))
                }
                "/read" | "/fork" => {
                    let pane =
                        self.commander_action_pane(target.ok_or(format!("Use {action} @pane"))?)?;
                    if words.next().is_some() {
                        return Err(format!("{action} takes one exact pane"));
                    }
                    if action == "/read" {
                        Ok(SlashAction::Read(pane))
                    } else {
                        Ok(SlashAction::Fork(pane))
                    }
                }
                "/split" => {
                    let pane =
                        self.commander_action_pane(target.ok_or("Use /split @pane right|below")?)?;
                    let axis = match words.next() {
                        Some("right" | "sv") => Axis::Col,
                        Some("below" | "sh") => Axis::Row,
                        _ => return Err("Use /split @pane right|below".into()),
                    };
                    if words.next().is_some() {
                        return Err("/split takes one pane and one direction".into());
                    }
                    Ok(SlashAction::Split(pane, axis))
                }
                "/task" | "/automation" | "/mission" | "/diff" | "/files" => {
                    if target.is_some() {
                        return Err(format!("{action} takes no inline arguments"));
                    }
                    Ok(match action {
                        "/task" => SlashAction::Task,
                        "/automation" => SlashAction::Automation,
                        "/mission" => SlashAction::Mission,
                        "/diff" => SlashAction::Diff,
                        _ => SlashAction::Files,
                    })
                }
                _ => Err(format!(
                    "Unknown action {action}. Type / to see available actions"
                )),
            }
        })())
    }

    fn commander_action_pane(&self, token: &str) -> Result<ActionPane, String> {
        if !token.starts_with('@') || token.len() == 1 {
            return Err("Use one exact @pane target".into());
        }
        let id = self.commander_resolve_target(target_lookup(token))?;
        let terminal_id = self
            .panes
            .get(&id)
            .and_then(|pane| pane.terminal_runtime())
            .ok_or_else(|| format!("p{} is not ready", id.0))?
            .terminal_id;
        Ok(ActionPane { id, terminal_id })
    }

    fn commander_focus_target(&self, token: &str) -> Result<FocusTarget, String> {
        if !token.starts_with('@') || token.len() == 1 {
            return Err("Use one exact @pane, @tab:name, or @workspace:name".into());
        }
        let lookup = target_lookup(token);
        if let Some(scope) = parse_scoped_target(lookup)? {
            if scope.pane.is_none() {
                let (workspace, tab) = self.commander_scope_indices(&scope)?;
                let workspace = workspace.ok_or("Choose a workspace or tab to focus")?;
                let ws = &self.workspaces[workspace];
                return if let Some(tab) = tab {
                    Ok(FocusTarget::Tab {
                        workspace,
                        tab,
                        workspace_id: ws.id.clone(),
                        tab_id: ws.tabs[tab].id.clone(),
                    })
                } else {
                    Ok(FocusTarget::Workspace {
                        index: workspace,
                        workspace_id: ws.id.clone(),
                    })
                };
            }
        }
        self.commander_resolve_target(lookup).map(FocusTarget::Pane)
    }

    fn commander_check_action_pane(&self, target: &ActionPane) -> Result<PaneId, String> {
        let current = self
            .panes
            .get(&target.id)
            .and_then(|pane| pane.terminal_runtime());
        if current.is_none_or(|runtime| runtime.terminal_id != target.terminal_id)
            || self.pane_location(target.id).is_none()
        {
            return Err(format!(
                "p{} changed or is no longer available",
                target.id.0
            ));
        }
        Ok(target.id)
    }

    pub(crate) fn commander_dispatch_slash_action(
        &mut self,
        action: SlashAction,
    ) -> Result<(), String> {
        match action {
            SlashAction::Focus(target) => {
                let (workspace, tab, pane) = match target {
                    FocusTarget::Pane(id) => {
                        let (workspace, tab) = self
                            .pane_location(id)
                            .filter(|_| self.panes.contains_key(&id))
                            .ok_or_else(|| format!("p{} is no longer available", id.0))?;
                        (workspace, tab, id)
                    }
                    FocusTarget::Tab {
                        workspace,
                        tab,
                        workspace_id,
                        tab_id,
                    } => {
                        let ws = self
                            .workspaces
                            .get(workspace)
                            .filter(|ws| ws.id == workspace_id)
                            .ok_or("Workspace changed before focus")?;
                        let selected = ws
                            .tabs
                            .get(tab)
                            .filter(|item| item.id == tab_id)
                            .ok_or("Tab changed before focus")?;
                        (workspace, tab, selected.layout.focus)
                    }
                    FocusTarget::Workspace {
                        index,
                        workspace_id,
                    } => {
                        let ws = self
                            .workspaces
                            .get(index)
                            .filter(|ws| ws.id == workspace_id)
                            .ok_or("Workspace changed before focus")?;
                        let tab = ws.active_tab;
                        let pane = ws
                            .tabs
                            .get(tab)
                            .ok_or("Workspace has no active tab")?
                            .layout
                            .focus;
                        (index, tab, pane)
                    }
                };
                self.focus_location(workspace, tab, pane);
                let mention = self
                    .panes
                    .contains_key(&pane)
                    .then(|| self.commander_pane_mention(pane));
                let commander = self.commander.as_mut().unwrap();
                commander.clear_all();
                commander.draft = mention.map_or_else(String::new, |mention| format!("{mention} "));
                commander.cursor = commander.draft.len();
                commander.focused = false;
                commander.receipt =
                    Some(format!("Focused workspace {} · tab {}", workspace, tab + 1));
                self.refresh_commander_preview();
            }
            SlashAction::Read(target) => {
                let id = self.commander_check_action_pane(&target)?;
                let text = self.pane_recent_text(id);
                let mut lines: Vec<String> = text
                    .lines()
                    .rev()
                    .take(READ_LINES)
                    .map(|line| line.chars().take(READ_LINE_CHARS).collect())
                    .collect();
                lines.reverse();
                if lines.is_empty() {
                    lines.push("No recent output".into());
                }
                let count = lines.len();
                let commander = self.commander.as_mut().unwrap();
                commander.delivery_results.clear();
                commander.read_output = Some(lines);
                commander.read_scroll = 0;
                commander.receipt = Some(format!("p{} · {count} lines · PgUp/PgDn", id.0));
                self.commander_height = self.commander_height.max(READ_HEIGHT);
            }
            SlashAction::Fork(target) => {
                let id = self.commander_check_action_pane(&target)?;
                let result = self
                    .fork_agent_pane(id, true)
                    .map_err(|error| match error {
                        AgentForkError::PaneNotFound | AgentForkError::SourceNotPaneTab => {
                            format!("p{} is no longer a live pane", id.0)
                        }
                        AgentForkError::UnsupportedAgent => {
                            format!("p{} does not support session forking", id.0)
                        }
                        AgentForkError::SessionUnknown => {
                            format!("p{} has no known native session to fork", id.0)
                        }
                        AgentForkError::SpawnFailed => {
                            format!("Could not start the fork beside p{}", id.0)
                        }
                    })?;
                let mention = self.commander_pane_mention(result.pane);
                let commander = self.commander.as_mut().unwrap();
                commander.clear_all();
                commander.draft = format!("{mention} ");
                commander.cursor = commander.draft.len();
                commander.focused = false;
                commander.receipt = Some(format!("Forked p{} into p{}", id.0, result.pane.0));
                self.refresh_commander_preview();
            }
            SlashAction::Split(target, axis) => {
                let id = self.commander_check_action_pane(&target)?;
                self.commander_split(id, axis);
            }
            SlashAction::Task => {
                self.open_orch_form_kind(OrchFormKind::Task);
                let commander = self.commander.as_mut().unwrap();
                commander.clear_all();
                commander.focused = false;
                commander.receipt = Some("Task form opened".into());
                commander.preview.clear();
            }
            SlashAction::Automation => {
                self.open_orch_form_kind(OrchFormKind::Automation);
                let commander = self.commander.as_mut().unwrap();
                commander.clear_all();
                commander.focused = false;
                commander.receipt = Some("Automation form opened".into());
            }
            SlashAction::Mission | SlashAction::Diff | SlashAction::Files => {
                let label = match action {
                    SlashAction::Mission => {
                        self.open_mission_control(self.active_ws);
                        "Mission Control"
                    }
                    SlashAction::Diff => {
                        self.focus_diff_list();
                        "DIFF"
                    }
                    _ => {
                        self.focus_files_tree();
                        "FILES"
                    }
                };
                let commander = self.commander.as_mut().unwrap();
                commander.clear_all();
                commander.focused = false;
                commander.receipt = Some(format!("Opened {label}"));
            }
        }
        Ok(())
    }
}
