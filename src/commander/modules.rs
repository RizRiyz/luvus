//! Opt-in `$` commands backed by existing module actions. Parsing and final
//! authority checks stay on the App owner thread; argv execution stays in the
//! module runner.

use std::path::PathBuf;

use serde_json::{json, Value};

use super::{parse_scoped_target, target_lookup, Commander};
use crate::app::App;
use crate::ids::PaneId;
use crate::module::context::Target;
use crate::module::manifest::{allowed_on, CommanderConfirmation, CommanderInput, CommanderTarget};
use crate::terminal::pty::Pane;

/// The complete invocation document is bounded independently of the editor's
/// character limit. The wire contract is version 1 and UTF-8 JSON on stdin.
pub(crate) const MAX_INVOCATION_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ModuleCommandSpec {
    pub module_id: String,
    pub module_name: String,
    pub root: PathBuf,
    pub action_id: String,
    pub argv: Vec<String>,
    pub name: String,
    pub title: String,
    pub target: CommanderTarget,
    pub input: CommanderInput,
    pub confirmation: CommanderConfirmation,
    pub short_unique: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct ModuleSuggestion<'a> {
    pub command: String,
    pub spec: &'a ModuleCommandSpec,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ModuleTarget {
    None,
    Pane {
        id: PaneId,
        terminal_id: String,
        workspace_id: String,
        tab_id: String,
        agent: bool,
    },
    Tab {
        workspace_id: String,
        tab_id: String,
    },
    Workspace {
        workspace_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ModuleInvocation {
    pub spec: ModuleCommandSpec,
    pub target: ModuleTarget,
    pub text: String,
    pub draft: String,
}

impl Commander {
    /// The catalog is a registry snapshot. Suggestions filter it without
    /// touching disk or module discovery during rendering.
    pub(crate) fn module_menu(&self) -> Option<(Vec<ModuleSuggestion<'_>>, usize)> {
        if !self.draft.starts_with('$') {
            return None;
        }
        let end = self
            .draft
            .find(char::is_whitespace)
            .unwrap_or(self.draft.len());
        if self.cursor > end {
            return None;
        }
        let typed = &self.draft[..end];
        let exact = self.module_commands.iter().any(|spec| {
            let short = format!("${}", spec.name);
            typed == format!("${}/{}", spec.module_id, spec.name)
                || (spec.short_unique && typed == short)
        });
        let matches: Vec<_> = self
            .module_commands
            .iter()
            .filter_map(|spec| {
                let short = format!("${}", spec.name);
                let qualified = format!("${}/{}", spec.module_id, spec.name);
                let ambiguous = !spec.short_unique;
                let short_matches = ambiguous && short.starts_with(typed);
                let command = if typed.contains('/') || ambiguous {
                    qualified
                } else {
                    short
                };
                (typed == "$" || exact || command.starts_with(typed) || short_matches)
                    .then_some(ModuleSuggestion { command, spec })
            })
            .collect();
        let default = matches
            .iter()
            .position(|suggestion| suggestion.command == typed)
            .unwrap_or(0);
        let selected = self
            .module_selection
            .unwrap_or(default)
            .min(matches.len().saturating_sub(1));
        Some((matches, selected))
    }
}

impl App {
    pub(crate) fn refresh_commander_module_catalog(&mut self) {
        let Some(commander) = self.commander.as_mut() else {
            return;
        };
        commander.module_commands = self
            .modules
            .modules
            .iter()
            .filter(|module| module.is_runnable())
            .flat_map(|module| {
                module
                    .manifest
                    .actions
                    .iter()
                    .filter(|action| allowed_on(action.platforms.as_ref()))
                    .filter_map(|action| {
                        let exposure = action.commander.as_ref()?;
                        Some(ModuleCommandSpec {
                            module_id: module.id.clone(),
                            module_name: module.manifest.name.clone(),
                            root: module.root.clone(),
                            action_id: action.id.clone(),
                            argv: action.command.clone(),
                            name: exposure.name.clone(),
                            title: action.title.clone(),
                            target: exposure.target,
                            input: exposure.input,
                            confirmation: exposure.confirmation,
                            short_unique: false,
                        })
                    })
            })
            .collect();
        let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for spec in &commander.module_commands {
            *counts.entry(spec.name.clone()).or_insert(0) += 1;
        }
        for spec in &mut commander.module_commands {
            spec.short_unique = counts.get(spec.name.as_str()) == Some(&1);
        }
        commander.module_selection = None;
    }

    pub(crate) fn commander_parse_module_command(
        &self,
        draft: &str,
    ) -> Option<Result<ModuleInvocation, String>> {
        if !draft.starts_with('$') {
            return None;
        }
        Some((|| {
            if self.commander.as_ref().is_some_and(|commander| {
                commander
                    .staged_images
                    .iter()
                    .any(|path| draft.contains(&path.to_string_lossy().to_string()))
            }) {
                return Err("Module commands do not accept staged clipboard images".into());
            }
            let (token, remaining) = split_word(draft);
            let name = token.strip_prefix('$').unwrap();
            if name.is_empty() {
                return Err("Choose a module command from the $ picker".into());
            }
            let catalog = &self
                .commander
                .as_ref()
                .ok_or("Commander is closed")?
                .module_commands;
            let matches: Vec<_> = catalog
                .iter()
                .filter(|spec| {
                    name == spec.name || name == format!("{}/{}", spec.module_id, spec.name)
                })
                .collect();
            let spec = match matches.as_slice() {
                [] => return Err(format!("No runnable module command ${name}")),
                [spec] => (*spec).clone(),
                _ => {
                    let choices = matches
                        .iter()
                        .map(|spec| format!("${}/{}", spec.module_id, spec.name))
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(format!("${name} is ambiguous: {choices}"));
                }
            };
            self.check_module_spec(&spec)?;
            let (target, text) = if spec.target == CommanderTarget::None {
                (ModuleTarget::None, remaining)
            } else {
                let (mention, text) = split_word(remaining.trim_start());
                if !mention.starts_with('@') || mention.len() == 1 {
                    return Err(format!(
                        "${} requires one exact @{} target",
                        spec.name,
                        match spec.target {
                            CommanderTarget::Agent => "agent-pane",
                            CommanderTarget::Tab => "tab:name",
                            CommanderTarget::Workspace => "workspace:name",
                            _ => "pane",
                        }
                    ));
                }
                (self.resolve_module_target(mention, spec.target)?, text)
            };
            if spec.input == CommanderInput::None && !text.trim().is_empty() {
                return Err(format!("${} does not take text input", spec.name));
            }
            let text = if spec.input == CommanderInput::Text {
                text.to_string()
            } else {
                String::new()
            };
            let invocation = ModuleInvocation {
                spec,
                target,
                text,
                draft: draft.to_string(),
            };
            // Reject oversized UTF-8 before process launch, including JSON
            // escaping overhead. The editor independently bounds characters.
            if serde_json::to_vec(&invocation.document())
                .map_err(|e| e.to_string())?
                .len()
                > MAX_INVOCATION_BYTES
            {
                return Err("Module command input exceeds 64 KiB".into());
            }
            Ok(invocation)
        })())
    }

    fn check_module_spec(&self, spec: &ModuleCommandSpec) -> Result<(), String> {
        let module = self
            .modules
            .find(&spec.module_id)
            .filter(|module| module.is_runnable())
            .ok_or_else(|| format!("Module {} is unavailable", spec.module_id))?;
        let action = module
            .manifest
            .actions
            .iter()
            .find(|action| action.id == spec.action_id)
            .ok_or_else(|| format!("Action {} is unavailable", spec.action_id))?;
        if !allowed_on(action.platforms.as_ref()) {
            return Err("Module action is unavailable on this platform".into());
        }
        let exposure = action
            .commander
            .as_ref()
            .ok_or("Module command is no longer exposed in Commander")?;
        if module.root != spec.root
            || module.manifest.name != spec.module_name
            || action.command != spec.argv
            || action.title != spec.title
            || exposure.name != spec.name
            || exposure.target != spec.target
            || exposure.input != spec.input
            || exposure.confirmation != spec.confirmation
        {
            return Err("Module command changed; select it again".into());
        }
        Ok(())
    }

    fn resolve_module_target(
        &self,
        mention: &str,
        kind: CommanderTarget,
    ) -> Result<ModuleTarget, String> {
        match kind {
            CommanderTarget::None => Ok(ModuleTarget::None),
            CommanderTarget::Pane | CommanderTarget::Agent => {
                let id = self.commander_resolve_target(target_lookup(mention))?;
                let (workspace, tab) = self
                    .pane_location(id)
                    .ok_or_else(|| format!("p{} is no longer in a tab", id.0))?;
                let agent = self.is_agent_pane(id);
                if kind == CommanderTarget::Agent && !agent {
                    return Err(format!("p{} is not a live agent pane", id.0));
                }
                let terminal_id = self
                    .panes
                    .get(&id)
                    .and_then(Pane::terminal_runtime)
                    .ok_or_else(|| format!("p{} is not ready", id.0))?
                    .terminal_id;
                Ok(ModuleTarget::Pane {
                    id,
                    terminal_id,
                    workspace_id: self.workspaces[workspace].id.clone(),
                    tab_id: self.workspaces[workspace].tabs[tab].id.clone(),
                    agent,
                })
            }
            CommanderTarget::Tab | CommanderTarget::Workspace => {
                let scope = parse_scoped_target(target_lookup(mention))?
                    .ok_or("Use an exact named workspace or tab mention")?;
                if scope.pane.is_some() {
                    return Err("This command does not target a pane".into());
                }
                let (workspace, tab) = self.commander_scope_indices(&scope)?;
                let workspace = workspace.ok_or("Choose an exact workspace")?;
                if kind == CommanderTarget::Tab {
                    let tab = tab.ok_or("Choose an exact tab")?;
                    Ok(ModuleTarget::Tab {
                        workspace_id: self.workspaces[workspace].id.clone(),
                        tab_id: self.workspaces[workspace].tabs[tab].id.clone(),
                    })
                } else if tab.is_some() {
                    Err("This command targets a workspace, not a tab".into())
                } else {
                    Ok(ModuleTarget::Workspace {
                        workspace_id: self.workspaces[workspace].id.clone(),
                    })
                }
            }
        }
    }

    fn check_module_target(&self, target: &ModuleTarget) -> Result<Target, String> {
        match target {
            ModuleTarget::None => Ok(Target::default()),
            ModuleTarget::Pane {
                id,
                terminal_id,
                workspace_id,
                tab_id,
                agent,
            } => {
                let (workspace, tab) = self
                    .pane_location(*id)
                    .ok_or_else(|| format!("p{} is no longer available", id.0))?;
                if self.workspaces[workspace].id != *workspace_id
                    || self.workspaces[workspace].tabs[tab].id != *tab_id
                    || self.is_agent_pane(*id) != *agent
                    || self
                        .panes
                        .get(id)
                        .and_then(Pane::terminal_runtime)
                        .is_none_or(|runtime| runtime.terminal_id != *terminal_id)
                {
                    return Err(format!("p{} changed before invocation", id.0));
                }
                Ok(Target {
                    workspace: Some(workspace),
                    tab: Some(tab),
                    pane: Some(*id),
                    selection: None,
                })
            }
            ModuleTarget::Tab {
                workspace_id,
                tab_id,
            } => {
                let (workspace, ws) = self
                    .workspaces
                    .iter()
                    .enumerate()
                    .find(|(_, ws)| ws.id == *workspace_id)
                    .ok_or("Workspace changed before invocation")?;
                let tab = ws
                    .tabs
                    .iter()
                    .position(|item| item.id == *tab_id)
                    .ok_or("Tab changed before invocation")?;
                Ok(Target::tab(workspace, tab))
            }
            ModuleTarget::Workspace { workspace_id } => {
                let index = self
                    .workspaces
                    .iter()
                    .position(|ws| ws.id == *workspace_id)
                    .ok_or("Workspace changed before invocation")?;
                Ok(Target::workspace(index))
            }
        }
    }

    pub(crate) fn commander_prepare_module(&mut self, draft: &str) {
        let result = self.commander_parse_module_command(draft);
        let Some(result) = result else { return };
        let result =
            result.and_then(|invocation| {
                let target = self.check_module_target(&invocation.target)?;
                self.check_module_spec(&invocation.spec)?;
                let confirmation = self
                    .commander
                    .as_ref()
                    .and_then(|commander| commander.pending_module_confirmation.as_ref());
                if invocation.spec.confirmation == CommanderConfirmation::Required
                    && confirmation != Some(&invocation)
                {
                    let preview: String = invocation
                        .text
                        .chars()
                        .take(80)
                        .map(|character| {
                            if character.is_control() {
                                ' '
                            } else {
                                character
                            }
                        })
                        .collect();
                    let summary =
                        format!(
                    "Confirm ${}/{} (action {}) · {} · {} bytes{} · Enter again (Esc cancels)",
                    invocation.spec.module_id,
                    invocation.spec.name,
                    invocation.spec.action_id,
                    invocation.target.label(),
                    invocation.text.len(),
                    if preview.is_empty() { String::new() } else { format!(" · {preview}") }
                );
                    let commander = self.commander.as_mut().unwrap();
                    commander.pending_module_confirmation = Some(invocation);
                    commander.receipt = Some(summary);
                    return Ok(None);
                }
                let document =
                    serde_json::to_vec(&invocation.document()).map_err(|e| e.to_string())?;
                if document.len() > MAX_INVOCATION_BYTES {
                    return Err("Module command input exceeds 64 KiB".into());
                }
                let log_id = self.run_module_command_for_input(
                    &invocation.spec.module_id,
                    invocation.spec.argv.clone(),
                    format!("action:{}", invocation.spec.action_id),
                    vec![(
                        "LUVUS_MODULE_ACTION_ID".into(),
                        invocation.spec.action_id.clone(),
                    )],
                    ("commander", target),
                    crate::module::runtime::StdinRequest {
                        bytes: document,
                        allow_closed_on_success: invocation.spec.input == CommanderInput::None,
                    },
                )?;
                Ok(Some((log_id, invocation)))
            });
        match result {
            Ok(Some((log_id, invocation))) => {
                let label = format!("${}/{}", invocation.spec.module_id, invocation.spec.name);
                let commander = self.commander.as_mut().unwrap();
                // Taken before clear_all so an earlier result held under this
                // confirmation is shown with the new start, not dropped.
                let held = commander.take_held();
                commander.clear_all();
                commander.running_modules.push((log_id, label.clone()));
                let started = format!("{label} started · log {log_id}");
                commander.receipt = Some(match held {
                    Some(held) => format!("{started} · {held}"),
                    None => started,
                });
            }
            Ok(None) => {}
            Err(error) => {
                let commander = self.commander.as_mut().unwrap();
                commander.pending_module_confirmation = None;
                commander.receipt = Some(error);
            }
        }
    }
}

fn split_word(input: &str) -> (&str, &str) {
    let end = input.find(char::is_whitespace).unwrap_or(input.len());
    let rest = input[end..]
        .chars()
        .next()
        .map_or("", |space| &input[end + space.len_utf8()..]);
    (&input[..end], rest)
}

impl ModuleInvocation {
    fn document(&self) -> Value {
        json!({
            "version": 1,
            "command": self.spec.name,
            "target": self.target.document(self.spec.target),
            "text": self.text,
        })
    }
}

impl ModuleTarget {
    fn document(&self, declared: CommanderTarget) -> Value {
        match self {
            ModuleTarget::None => json!({"kind":"none"}),
            ModuleTarget::Pane { id, .. } => json!({
                "kind": if declared == CommanderTarget::Agent { "agent" } else { "pane" },
                "pane_id": id.0.to_string(),
            }),
            ModuleTarget::Tab {
                workspace_id,
                tab_id,
            } => json!({
                "kind":"tab", "workspace_id":workspace_id, "tab_id":tab_id,
            }),
            ModuleTarget::Workspace { workspace_id } => json!({
                "kind":"workspace", "workspace_id":workspace_id,
            }),
        }
    }

    pub(crate) fn label(&self) -> String {
        match self {
            ModuleTarget::None => "no target".into(),
            ModuleTarget::Pane { id, .. } => format!("p{}", id.0),
            ModuleTarget::Tab { tab_id, .. } => format!("tab {tab_id}"),
            ModuleTarget::Workspace { workspace_id } => format!("workspace {workspace_id}"),
        }
    }
}
