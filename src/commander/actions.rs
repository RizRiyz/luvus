//! Typed Commander slash actions. These reuse App operations directly; slash
//! text is never passed to a terminal or interpreted as a CLI command.

use super::orch;
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
        usage: "[@target] · Enter form · Tab browse",
        needs_target: false,
    },
    SlashActionSpec {
        name: "/automation",
        usage: "[@target] · Enter form · Tab browse",
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
    Task(Option<FormTarget>, OrchActionMode),
    Automation(Option<FormTarget>, OrchActionMode),
    Mission,
    Diff,
    Files,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum OrchActionMode {
    Guided,
    Modal,
    Submit,
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

#[derive(Debug)]
pub(crate) enum FormTarget {
    Workspace {
        id: String,
        name: String,
    },
    Agent {
        pane: PaneId,
        terminal_id: String,
        workspace_id: String,
        workspace_name: String,
        agent: String,
    },
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
            let (header, guided_fields) = trimmed
                .split_once('\n')
                .map_or((trimmed, false), |(header, _)| (header, true));
            let mut words = header.split_whitespace();
            let action = words.next().unwrap();
            let target = words.next();
            if guided_fields && !matches!(action, "/task" | "/automation") {
                return Err(format!("{action} does not take a guided draft"));
            }
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
                "/task" | "/automation" => {
                    let rest = trimmed[action.len()..].trim_start();
                    let first = rest.split_whitespace().next();
                    let (form_target, fields) =
                        if let Some(token) = first.filter(|t| t.starts_with('@')) {
                            (
                                Some(self.commander_form_target(token)?),
                                rest[token.len()..].trim_start(),
                            )
                        } else {
                            (None, rest)
                        };
                    let mode = match fields {
                        "" | "form" => OrchActionMode::Modal,
                        "guide" => OrchActionMode::Guided,
                        _ if orch::has_inline_fields(trimmed) => OrchActionMode::Submit,
                        _ => return Err(format!("Use {action} [@target] [field: value ...]")),
                    };
                    Ok(if action == "/task" {
                        SlashAction::Task(form_target, mode)
                    } else {
                        SlashAction::Automation(form_target, mode)
                    })
                }
                "/mission" | "/diff" | "/files" => {
                    if target.is_some() {
                        return Err(format!("{action} takes no inline arguments"));
                    }
                    Ok(match action {
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

    fn commander_form_target(&self, token: &str) -> Result<FormTarget, String> {
        if !token.starts_with('@') || token.len() == 1 {
            return Err("Use @workspace:name or an exact @agent-pane".into());
        }
        let lookup = target_lookup(token);
        if let Some(scope) = parse_scoped_target(lookup)? {
            if scope.pane.is_none() {
                let (workspace, _) = self.commander_scope_indices(&scope)?;
                let workspace = workspace.ok_or("Choose an exact workspace")?;
                let workspace = &self.workspaces[workspace];
                return Ok(FormTarget::Workspace {
                    id: workspace.id.clone(),
                    name: workspace.name.clone(),
                });
            }
        }
        let target = self.commander_action_pane(token)?;
        let (workspace, _) = self
            .pane_location(target.id)
            .ok_or("Agent pane is no longer in a workspace")?;
        let status = self
            .status
            .get(&target.id)
            .filter(|_| self.is_agent_pane(target.id))
            .ok_or("Task and automation pane targets must be running agents")?;
        let workspace = &self.workspaces[workspace];
        Ok(FormTarget::Agent {
            pane: target.id,
            terminal_id: target.terminal_id,
            workspace_id: workspace.id.clone(),
            workspace_name: workspace.name.clone(),
            agent: status.agent.clone(),
        })
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
            SlashAction::Task(target, mode) => {
                self.commander_orch_action(OrchFormKind::Task, target, mode)?
            }
            SlashAction::Automation(target, mode) => {
                self.commander_orch_action(OrchFormKind::Automation, target, mode)?
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

    fn commander_orch_action(
        &mut self,
        kind: OrchFormKind,
        mut target: Option<FormTarget>,
        mode: OrchActionMode,
    ) -> Result<(), String> {
        if matches!(mode, OrchActionMode::Submit)
            && self
                .commander
                .as_ref()
                .and_then(|commander| commander.guided_orch)
                .is_some_and(|draft_kind| draft_kind != kind)
        {
            return Err("Guided action changed; restart the draft".into());
        }
        let original_binding = self
            .commander
            .as_ref()
            .and_then(|commander| commander.guided_binding.clone());
        if matches!(mode, OrchActionMode::Submit) && target.is_none() {
            if let Some(binding) = &original_binding {
                let workspace = self
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.id == binding.workspace_id)
                    .ok_or("Guided draft workspace is no longer available")?;
                target = Some(FormTarget::Workspace {
                    id: workspace.id.clone(),
                    name: workspace.name.clone(),
                });
            }
        }
        let pane_binding = match &target {
            Some(FormTarget::Agent {
                pane, terminal_id, ..
            }) => Some((*pane, terminal_id.clone())),
            _ => None,
        };
        self.open_orch_form_kind(kind);
        if let Err(error) = self.commander_bind_orch_form(target, kind == OrchFormKind::Automation)
        {
            self.orch_form = None;
            return Err(error);
        }
        let binding = orch::GuidedBinding {
            workspace_id: self
                .orch_form
                .as_ref()
                .and_then(|form| form.workspace_id.clone())
                .ok_or("Guided draft needs an exact workspace")?,
            pane: pane_binding,
        };
        if matches!(mode, OrchActionMode::Submit)
            && original_binding
                .as_ref()
                .is_some_and(|original| original != &binding)
        {
            self.orch_form = None;
            return Err("Guided target changed; restart the draft".into());
        }
        match mode {
            OrchActionMode::Guided => {
                let header = self
                    .commander
                    .as_ref()
                    .unwrap()
                    .draft
                    .trim()
                    .strip_suffix(" guide")
                    .unwrap_or_default()
                    .to_string();
                let draft = orch::template(&header, self.orch_form.as_ref().unwrap());
                let first_field = orch::field_positions(&draft)
                    .first()
                    .copied()
                    .unwrap_or(draft.len());
                let prior_height = self.commander_height;
                self.commander_height =
                    prior_height.max((draft.lines().count() as u16 + 3).min(16));
                self.orch_form = None;
                let commander = self.commander.as_mut().unwrap();
                commander.clear_all();
                commander.draft = draft;
                commander.cursor = first_field;
                commander.guided_orch = Some(kind);
                commander.guided_prior_height = Some(prior_height);
                commander.guided_binding = Some(binding);
                commander.receipt = None;
            }
            OrchActionMode::Modal => {
                let prior_height = self.commander.as_ref().unwrap().guided_prior_height;
                let commander = self.commander.as_mut().unwrap();
                commander.clear_all();
                commander.focused = false;
                commander.receipt = Some(format!(
                    "{} form opened; creation waits for the form",
                    if kind == OrchFormKind::Task {
                        "Task"
                    } else {
                        "Automation"
                    }
                ));
                commander.preview.clear();
                if let Some(height) = prior_height {
                    self.commander_height = height;
                }
            }
            OrchActionMode::Submit => {
                let draft = self.commander.as_ref().unwrap().draft.clone();
                if let Err(error) = orch::parse_into(&draft, self.orch_form.as_mut().unwrap()) {
                    self.orch_form = None;
                    return Err(error);
                }
                self.submit_orch_form();
                if let Some(form) = self.orch_form.take() {
                    return Err(form
                        .error
                        .unwrap_or_else(|| "Could not create the ORCH item".into()));
                }
                let prior_height = self.commander.as_ref().unwrap().guided_prior_height;
                let receipt = self.commander.as_ref().unwrap().receipt.clone();
                let commander = self.commander.as_mut().unwrap();
                commander.release_staged_images();
                commander.clear_all();
                commander.receipt = receipt;
                commander.focused = true;
                if let Some(height) = prior_height {
                    self.commander_height = height;
                }
            }
        }
        Ok(())
    }

    fn commander_bind_orch_form(
        &mut self,
        target: Option<FormTarget>,
        automation: bool,
    ) -> Result<(), String> {
        let (workspace_id, label, agent_target) = match target {
            Some(FormTarget::Workspace { id, name }) => (id, name, None),
            Some(FormTarget::Agent {
                pane,
                terminal_id,
                workspace_id,
                workspace_name,
                agent,
            }) => {
                let still_live = self
                    .panes
                    .get(&pane)
                    .and_then(|pane| pane.terminal_runtime())
                    .is_some_and(|runtime| runtime.terminal_id == terminal_id);
                if !still_live || !self.is_agent_pane(pane) {
                    self.orch_form = None;
                    return Err("Agent pane changed before the form opened".into());
                }
                (
                    workspace_id,
                    workspace_name,
                    Some((pane, terminal_id, agent)),
                )
            }
            None => {
                let workspace = self
                    .workspaces
                    .get(self.active_ws)
                    .ok_or("No active workspace")?;
                (workspace.id.clone(), workspace.name.clone(), None)
            }
        };
        if !self
            .workspaces
            .iter()
            .any(|workspace| workspace.id == workspace_id)
        {
            return Err("Selected workspace is no longer available".into());
        }
        let form = self.orch_form.as_mut().expect("form was opened");
        if automation {
            form.active_agents.retain(|choice| {
                choice.workspace_id == workspace_id
                    && agent_target.as_ref().is_none_or(|(pane, terminal_id, _)| {
                        choice.pane == *pane && choice.terminal_id == *terminal_id
                    })
            });
            if agent_target.is_some() && form.active_agents.is_empty() {
                return Err("Agent is no longer available for automation".into());
            }
        }
        form.workspace_id = Some(workspace_id);
        form.commander_origin = true;
        form.commander_target_label = Some(label);
        if let Some((pane, terminal_id, agent)) = agent_target {
            if automation {
                form.automation_target = crate::app::OrchAutomationTarget::ActiveAgent;
                form.active_agent = form
                    .active_agents
                    .iter()
                    .position(|choice| choice.pane == pane && choice.terminal_id == terminal_id)
                    .ok_or("Agent is no longer available for automation")?;
            } else {
                form.agent = agent;
            }
        }
        Ok(())
    }
}
