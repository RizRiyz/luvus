//! Pure JSON projections for control API responses.

use super::*;

pub(in crate::app::dispatch) fn diff_file_json(file: &crate::diff::DiffFile) -> Value {
    json!({
        "path":file.key.display_path(),
        "path_raw_hex":file.key.new_path.as_ref().or(file.key.old_path.as_ref()).map(|path| path.raw_hex.as_str()),
        "old_path":file.key.old_path.as_ref().map(|path| path.display.as_str()),
        "old_path_raw_hex":file.key.old_path.as_ref().map(|path| path.raw_hex.as_str()),
        "layer":file.key.layer.label(),
        "status":file.status.badge(),
        "additions":file.additions,
        "deletions":file.deletions,
        "binary":file.binary,
        "notes":file.unresolved_notes,
        "viewed":file.viewed(),
        "modified_since_review":file.modified_since_review(),
        "fingerprint":file.fingerprint,
    })
}

pub(in crate::app::dispatch) fn note_state_label(state: crate::diff::NoteState) -> &'static str {
    match state {
        crate::diff::NoteState::Open => "open",
        crate::diff::NoteState::Resolved => "resolved",
        crate::diff::NoteState::Outdated => "outdated",
        crate::diff::NoteState::Orphaned => "orphaned",
    }
}

pub(in crate::app::dispatch) fn note_json(note: &crate::diff::ReviewNote) -> Value {
    json!({
        "id":note.id,
        "review":note.review_id,
        "author":note.author,
        "kind":note.kind.label(),
        "body":note.body,
        "state":note_state_label(note.state),
        "path":note.anchor.diff_key.display_path(),
        "layer":note.anchor.diff_key.layer.label(),
        "side":note.anchor.side.label(),
        "start_line":note.anchor.start_line,
        "end_line":note.anchor.end_line,
        "revision":note.revision,
        "deliveries":note.deliveries,
        "created_at_ms":note.created_at_ms,
        "updated_at_ms":note.updated_at_ms,
    })
}

/// A `Task` as a JSON value for API results + bus events.
pub(crate) fn task_json(t: &crate::orch::Task) -> Value {
    let mut value = json!({
        "id": t.id,
        "title": t.title,
        "status": t.status,
        "assignee": t.assignee,
        "deps": t.deps,
        "paths": t.paths,
        "gate": t.gate,
        "outputs": t.outputs,
        "notes": t.notes,
        "worktree": t.worktree,
        "branch": t.branch,
        "context": t.context,
        "created": t.created,
        "updated": t.updated,
    });
    // Manual task briefings are part of the ORCH task contract. Automation
    // prompts remain private to their definition/run projection and must not
    // leak through the general task list or event stream.
    if t.automation.is_none() {
        value["prompt"] = json!(t.prompt);
    }
    if let Some(mode) = t.worker_mode {
        value["mode"] = json!(mode);
    }
    if let Some(workspace) = &t.workspace_worker {
        value["workspace_worker"] = json!(workspace);
    }
    value
}

/// A trimmed JSON view of an installed module for `module.list`.
pub(in crate::app::dispatch) fn module_json(m: &crate::module::InstalledModule) -> Value {
    json!({
        "id": m.id,
        "name": m.manifest.name,
        "version": m.manifest.version,
        "enabled": m.enabled,
        "runnable": m.is_runnable(),
        "root": m.root.display().to_string(),
        "source": m.source,
        "actions": m.manifest.actions.iter().map(|a| a.id.clone()).collect::<Vec<_>>(),
        "panes": m.manifest.panes.iter().map(|pe| pe.id.clone()).collect::<Vec<_>>(),
        "bars": m.manifest.bars.iter().map(|bar| bar.id.clone()).collect::<Vec<_>>(),
        "warning": m.warning,
    })
}

/// Privacy-preserving executable inventory from cached process command lines.
/// Keep only argv[0], plus an interpreter's first non-flag script name, and
/// de-duplicate in scan order. Full argv commonly contains prompts or secrets.
pub(in crate::app::dispatch) fn process_executables(commands: &[String]) -> Vec<String> {
    let mut result = Vec::new();
    for command in commands.iter().take(128) {
        let mut words = command.split_whitespace();
        let Some(first) = words.next() else { continue };
        let first = crate::detect::binary_name(first);
        if !first.is_empty() && !result.iter().any(|item| item == first) {
            result.push(first.to_string());
        }
        if crate::detect::is_interpreter(first) {
            if let Some(script) = words.find(|word| !word.starts_with('-')) {
                let script = crate::detect::binary_name(script);
                if !script.is_empty() && !result.iter().any(|item| item == script) {
                    result.push(script.to_string());
                }
            }
        }
    }
    result
}

pub(crate) fn state_str(s: State) -> &'static str {
    match s {
        State::Blocked => "blocked",
        State::Working => "working",
        State::Done => "done",
        State::Idle => "idle",
        State::Unknown => "unknown",
    }
}
