//! Inline and guided ORCH drafts inside Commander. Fields are copied into the
//! existing validated form; no shell parsing or CLI subprocesses are involved.

use crate::app::{OrchAutomationTarget, OrchForm, OrchFormKind, OrchFormStart};
use crate::automation::AutomationAccess;
use crate::ids::PaneId;
use crate::orch::TaskWorkerMode;
use std::collections::HashSet;
use std::ops::Range;

const FIELD_NAMES: &[&str] = &[
    "title", "start", "schedule", "timezone", "agent", "mode", "access", "paths", "deps", "gate",
    "prompt",
];

#[derive(Debug)]
pub(super) struct InlineField {
    pub name: &'static str,
    pub marker: usize,
    pub value: Range<usize>,
    pub raw_end: usize,
}

pub(super) enum InlineTab {
    Replace(Range<usize>, String),
    Move(usize),
    Noop,
}

const TASK_FIELDS: &[&str] = &[
    "title", "start", "agent", "mode", "paths", "deps", "gate", "prompt",
];
const AUTOMATION_FIELDS: &[&str] = &[
    "title", "start", "schedule", "timezone", "agent", "mode", "access", "paths", "gate", "prompt",
];
const ACTIVE_AUTOMATION_FIELDS: &[&str] = &["title", "start", "schedule", "timezone", "prompt"];

pub(super) fn is_choice_field(name: &str) -> bool {
    matches!(
        name,
        "start" | "schedule" | "timezone" | "agent" | "mode" | "access"
    )
}

pub(super) fn next_field_text(
    draft: &str,
    kind: OrchFormKind,
    active_agent: bool,
    selected_agent: Option<&str>,
) -> Option<String> {
    let order = match (kind, active_agent) {
        (OrchFormKind::Task, _) => TASK_FIELDS,
        (OrchFormKind::Automation, true) => ACTIVE_AUTOMATION_FIELDS,
        (OrchFormKind::Automation, false) => AUTOMATION_FIELDS,
    };
    let fields = inline_fields(draft);
    let next = if let Some(last) = fields.last() {
        order.iter().position(|name| *name == last.name)? + 1
    } else {
        0
    };
    let name = order.get(next)?;
    let value = if *name == "agent" {
        selected_agent.unwrap_or("")
    } else {
        ""
    };
    Some(format!(
        "{}{}: {}",
        match (fields.is_empty(), draft.ends_with(char::is_whitespace)) {
            (true, true) => "",
            (true, false) | (false, true) => " ",
            (false, false) => "  ",
        },
        name,
        value
    ))
}

fn body_start(draft: &str) -> Option<usize> {
    let action = draft.split_whitespace().next()?;
    if !matches!(action, "/task" | "/automation") {
        return None;
    }
    let mut offset = draft.len() - draft.trim_start().len() + action.len();
    offset += draft[offset..].len() - draft[offset..].trim_start().len();
    if draft[offset..].starts_with('@') {
        let end = draft[offset..]
            .find(char::is_whitespace)
            .map_or(draft.len(), |end| offset + end);
        offset = end;
        offset += draft[offset..].len() - draft[offset..].trim_start().len();
    }
    Some(offset)
}

/// Recognize explicit `field:` markers at word boundaries. `prompt:` consumes
/// the remainder, so ordinary prompt prose is never reinterpreted as fields.
pub(super) fn inline_fields(draft: &str) -> Vec<InlineField> {
    let Some(start) = body_start(draft) else {
        return Vec::new();
    };
    let mut markers = Vec::new();
    for (relative, _) in draft[start..].char_indices() {
        let index = start + relative;
        if index != start
            && !draft[..index]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace)
        {
            continue;
        }
        if let Some(&name) = FIELD_NAMES.iter().find(|name| {
            draft[index..].starts_with(**name) && draft[index + name.len()..].starts_with(':')
        }) {
            markers.push((name, index));
            if name == "prompt" {
                break;
            }
        }
    }
    markers
        .iter()
        .enumerate()
        .map(|(index, &(name, marker))| {
            let value_start = marker + name.len() + 1;
            let end = markers
                .get(index + 1)
                .map_or(draft.len(), |(_, start)| *start);
            let mut value_end = end;
            while value_end > value_start && draft.as_bytes()[value_end - 1].is_ascii_whitespace() {
                value_end -= 1;
            }
            InlineField {
                name,
                marker,
                value: value_start..value_end,
                raw_end: end,
            }
        })
        .collect()
}

pub(super) fn has_inline_fields(draft: &str) -> bool {
    body_start(draft).is_some_and(|start| {
        inline_fields(draft)
            .first()
            .is_some_and(|field| field.marker == start)
    })
}

pub(super) fn inline_field_at(draft: &str, cursor: usize) -> Option<InlineField> {
    inline_fields(draft)
        .into_iter()
        .find(|field| field.value.start <= cursor && cursor <= field.raw_end)
}

pub(super) fn unfinished_choice(draft: &str, field: &InlineField) -> bool {
    let kind = match draft.split_whitespace().next() {
        Some("/task") => OrchFormKind::Task,
        Some("/automation") => OrchFormKind::Automation,
        _ => return false,
    };
    let current = draft[field.value.clone()].trim();
    !current.is_empty()
        && choice_values(kind, field.name, draft)
            .iter()
            .any(|choice| choice.starts_with(current) && choice != current)
}

fn choice_values(kind: OrchFormKind, name: &str, draft: &str) -> Vec<String> {
    match name {
        "start" if kind == OrchFormKind::Task => ["manual", "now"].map(str::to_owned).to_vec(),
        "start" => ["once", "hourly", "daily", "weekly"]
            .map(str::to_owned)
            .to_vec(),
        "schedule" if kind == OrchFormKind::Automation => {
            let fields = inline_fields(draft);
            let start = fields
                .iter()
                .find(|field| field.name == "start")
                .map(|field| draft[field.value.clone()].trim());
            match start {
                Some("once") => {
                    let timezone = fields
                        .iter()
                        .find(|field| field.name == "timezone")
                        .map(|field| draft[field.value.clone()].trim())
                        .filter(|value| !value.is_empty())
                        .map(str::to_owned)
                        .unwrap_or_else(crate::automation::system_timezone_name);
                    [3_600, 7_200, 86_400]
                        .into_iter()
                        .filter_map(|seconds| {
                            crate::automation::format_local_instant(
                                crate::automation::unix_now().saturating_add(seconds),
                                &timezone,
                            )
                            .ok()
                        })
                        .collect()
                }
                Some("hourly") => ["00", "15", "30", "45"].map(str::to_owned).to_vec(),
                Some("daily") => ["09:00", "12:00", "18:00"].map(str::to_owned).to_vec(),
                Some("weekly") => ["mon 09:00", "wed 09:00", "fri 09:00"]
                    .map(str::to_owned)
                    .to_vec(),
                _ => Vec::new(),
            }
        }
        "mode" => ["worktree", "workspace"].map(str::to_owned).to_vec(),
        "access" => ["read_only", "workspace", "full_access"]
            .map(str::to_owned)
            .to_vec(),
        "agent" => {
            let agents = if kind == OrchFormKind::Task {
                crate::app::task_agent_choices()
            } else {
                crate::app::automation_agent_choices()
            };
            agents.iter().map(|agent| (*agent).to_owned()).collect()
        }
        "timezone" => {
            static SYSTEM_TIMEZONE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
            let system = SYSTEM_TIMEZONE.get_or_init(crate::automation::system_timezone_name);
            let mut zones = vec![system.clone()];
            for zone in [
                "UTC",
                "Asia/Makassar",
                "Asia/Jakarta",
                "Asia/Singapore",
                "Asia/Tokyo",
                "Europe/London",
                "Europe/Berlin",
                "America/New_York",
                "America/Los_Angeles",
            ] {
                if zone != system {
                    zones.push(zone.to_owned());
                }
            }
            zones
        }
        _ => Vec::new(),
    }
}

/// Tab cycles bounded choices in place. Free-text fields move to the next
/// field, while any valid IANA timezone remains editable as ordinary text.
pub(super) fn inline_tab(draft: &str, cursor: usize, backward: bool) -> Option<InlineTab> {
    let field = inline_field_at(draft, cursor)?;
    let kind = match draft.split_whitespace().next()? {
        "/task" => OrchFormKind::Task,
        "/automation" => OrchFormKind::Automation,
        _ => return None,
    };
    let choices = choice_values(kind, field.name, draft);
    let current = draft[field.value.clone()].trim();
    if !choices.is_empty() {
        let index = choices
            .iter()
            .position(|choice| choice.eq_ignore_ascii_case(current));
        let chosen = if let Some(index) = index {
            (index + if backward { choices.len() - 1 } else { 1 }) % choices.len()
        } else if current.is_empty() {
            if backward {
                choices.len() - 1
            } else {
                0
            }
        } else if let Some(index) = choices
            .iter()
            .position(|choice| choice.starts_with(current))
        {
            index
        } else if matches!(field.name, "schedule" | "timezone") {
            return Some(InlineTab::Noop);
        } else {
            if backward {
                choices.len() - 1
            } else {
                0
            }
        };
        let end = if field.raw_end == draft.len() {
            field.raw_end
        } else {
            field.value.end
        };
        return Some(InlineTab::Replace(
            field.value.start..end,
            format!(" {}", choices[chosen]),
        ));
    }
    let fields = inline_fields(draft);
    let index = fields
        .iter()
        .position(|candidate| candidate.marker == field.marker)?;
    let next = if backward {
        fields.get(index.wrapping_sub(1))
    } else {
        fields.get(index + 1)
    };
    Some(next.map_or(InlineTab::Noop, |field| InlineTab::Move(field.value.start)))
}

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
    let start = body_start(draft).ok_or("Use /task or /automation")?;
    let fields = inline_fields(draft);
    if fields.first().is_none_or(|field| field.marker != start) {
        return Err("Use field: value after the action and optional @target".into());
    }
    let mut seen = HashSet::new();
    let mut prompt: Option<String> = None;
    for field in fields {
        let name = field.name;
        let value = draft[field.value].trim();
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
