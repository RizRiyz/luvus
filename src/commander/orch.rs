//! Guided multiline ORCH drafts inside Commander. No shell parsing or CLI
//! subprocesses: fields are copied into the existing validated ORCH form.

use crate::app::{OrchAutomationTarget, OrchForm, OrchFormKind, OrchFormStart};
use crate::automation::AutomationAccess;
use crate::ids::PaneId;
use crate::orch::TaskWorkerMode;
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct GuidedBinding {
    pub workspace_id: String,
    pub pane: Option<(PaneId, String)>,
}

pub(super) fn template(header: &str, form: &OrchForm) -> String {
    let mut draft = format!("{header}\ntitle: \n");
    if form.kind == OrchFormKind::Task {
        draft.push_str(&format!(
            "start: manual\nagent: {}\nmode: {}\npaths: \ndeps: \ngate: \nprompt: ",
            form.agent,
            form.mode.as_str()
        ));
    } else if form.automation_target == OrchAutomationTarget::ActiveAgent {
        draft.push_str(&format!(
            "start: once\nschedule: \ntimezone: {}\nprompt: ",
            form.timezone
        ));
    } else {
        draft.push_str(&format!(
            "start: once\nschedule: \ntimezone: {}\nagent: {}\nmode: {}\naccess: {}\npaths: \ngate: \nprompt: ",
            form.timezone,
            form.agent,
            form.mode.as_str(),
            form.access.as_str()
        ));
    }
    draft
}

/// Byte positions immediately after each editable field's colon and space.
pub(super) fn field_positions(draft: &str) -> Vec<usize> {
    let mut positions = Vec::new();
    let mut offset = draft.find('\n').map_or(draft.len(), |index| index + 1);
    for line in draft[offset..].split_inclusive('\n') {
        if let Some((name, _)) = line.split_once(':') {
            if matches!(
                name,
                "title"
                    | "start"
                    | "agent"
                    | "mode"
                    | "access"
                    | "schedule"
                    | "timezone"
                    | "paths"
                    | "deps"
                    | "gate"
                    | "prompt"
            ) {
                positions.push(
                    offset + name.len() + 1 + usize::from(line[name.len() + 1..].starts_with(' ')),
                );
                if name == "prompt" {
                    break;
                }
            }
        }
        offset += line.len();
    }
    positions
}

pub(super) fn parse_into(draft: &str, form: &mut OrchForm) -> Result<(), String> {
    let (_, fields) = draft
        .split_once('\n')
        .ok_or("Start a guided draft before submitting")?;
    let mut seen = HashSet::new();
    let mut prompt: Option<String> = None;
    for line in fields.lines() {
        if let Some(value) = prompt.as_mut() {
            value.push('\n');
            value.push_str(line);
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| format!("Expected a field: value line: {line}"))?;
        let value = value.strip_prefix(' ').unwrap_or(value);
        if !seen.insert(name) {
            return Err(format!("{name} was given more than once"));
        }
        match name {
            "title" => form.title = value.to_string(),
            "prompt" => prompt = Some(value.to_string()),
            "agent" => form.agent = value.to_string(),
            "mode" => {
                form.mode =
                    TaskWorkerMode::parse(value).ok_or("mode must be worktree or workspace")?
            }
            "access" => {
                form.access = AutomationAccess::parse(value)
                    .ok_or("access must be read_only, workspace, or full_access")?
            }
            "start" => {
                form.start = match (form.kind, value) {
                    (OrchFormKind::Task, "manual") => OrchFormStart::Manual,
                    (OrchFormKind::Task, "now") => OrchFormStart::Now,
                    (OrchFormKind::Automation, "once") => OrchFormStart::Once,
                    (OrchFormKind::Automation, "hourly") => OrchFormStart::Hourly,
                    (OrchFormKind::Automation, "daily") => OrchFormStart::Daily,
                    (OrchFormKind::Automation, "weekly") => OrchFormStart::Weekly,
                    (OrchFormKind::Task, _) => {
                        return Err("Task start must be manual or now".into())
                    }
                    (OrchFormKind::Automation, _) => {
                        return Err("Automation start must be once, hourly, daily, or weekly".into())
                    }
                }
            }
            "schedule" => {
                form.schedule = value.to_string();
                form.schedule_prefilled = false;
            }
            "timezone" => form.timezone = value.to_string(),
            "paths" => form.paths = value.to_string(),
            "deps" => form.deps = value.to_string(),
            "gate" => form.gate = value.to_string(),
            _ => return Err(format!("Unknown guided field {name}")),
        }
    }
    if !seen.contains("title") || form.title.trim().is_empty() {
        return Err("Enter a title in the guided draft".into());
    }
    if form.kind == OrchFormKind::Task {
        if seen.contains("access") || seen.contains("schedule") || seen.contains("timezone") {
            return Err("Task drafts cannot set access, schedule, or timezone".into());
        }
    } else {
        if seen.contains("deps") {
            return Err("Automation drafts cannot set deps".into());
        }
        if !seen.contains("schedule") || form.schedule.trim().is_empty() {
            return Err("Enter a schedule in the guided draft".into());
        }
        if form.automation_target == OrchAutomationTarget::ActiveAgent
            && ["agent", "mode", "access", "paths", "gate"]
                .iter()
                .any(|name| seen.contains(name))
        {
            return Err("An active-agent automation cannot set new-worker fields".into());
        }
    }
    if let Some(prompt) = prompt {
        form.prompt = prompt;
    }
    Ok(())
}
