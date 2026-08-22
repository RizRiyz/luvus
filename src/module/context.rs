//! The `LUVUS_MODULE_CONTEXT_JSON` blob: a snapshot of the workspace / tab /
//! pane a module command was invoked against (docs/13 §3.4).
//!
//! Most invocations (CLI, socket, event hooks) target whatever is focused. A
//! right-click menu instead targets the row or pane that was clicked, which may
//! not be the focused one — hence [`Target`].

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};

use crate::app::App;
use crate::ids::PaneId;
use crate::ui::theme::State;

static CORRELATION: AtomicU64 = AtomicU64::new(1);

/// What a module command should act on. All-`None` means "whatever is focused",
/// which is the right answer for CLI, socket, startup, and event invocations.
#[derive(Default, Clone)]
pub struct Target {
    pub workspace: Option<usize>,
    /// Explicit zero-based tab within `workspace`, for a tab context-menu action.
    pub tab: Option<usize>,
    pub pane: Option<PaneId>,
    /// The current mouse selection, when a menu was opened over one.
    pub selection: Option<String>,
}

impl Target {
    pub fn workspace(index: usize) -> Self {
        Target {
            workspace: Some(index),
            ..Default::default()
        }
    }

    pub fn pane(id: PaneId) -> Self {
        Target {
            pane: Some(id),
            ..Default::default()
        }
    }

    pub fn tab(workspace: usize, tab: usize) -> Self {
        Target {
            workspace: Some(workspace),
            tab: Some(tab),
            ..Default::default()
        }
    }
}

/// Build the context for a command invoked from `source` (cli|api|event|menu:*)
/// against the focused workspace/pane.
pub fn build(app: &App, source: &str) -> Value {
    build_for(app, source, &Target::default())
}

/// Build the context for a command invoked against an explicit `target`.
pub fn build_for(app: &App, source: &str, target: &Target) -> Value {
    let cid = format!("c{}", CORRELATION.fetch_add(1, Ordering::Relaxed));
    let ws_id = target
        .workspace
        .filter(|i| *i < app.workspaces.len())
        .unwrap_or(app.active_ws);
    let ws = app.workspaces.get(ws_id);
    let name = ws.map(|w| w.name.clone()).unwrap_or_default();
    let ws_cwd = ws.map(|w| w.cwd.display().to_string()).unwrap_or_default();
    let branch = ws.and_then(|w| w.branch.clone()).unwrap_or_default();
    let tab_id = ws
        .map(|w| {
            target
                .tab
                .filter(|index| *index < w.tabs.len())
                .unwrap_or(w.active_tab)
        })
        .unwrap_or(0);
    let tab_index = tab_id + 1;
    let tab_name = ws
        .and_then(|w| w.tabs.get(tab_id))
        .and_then(|t| t.name.clone())
        .unwrap_or_default();

    // A targeted pane wins, but only while it still exists (a menu can outlive
    // its pane if the process exits between the right-click and the click).
    let focus = target
        .pane
        .filter(|id| app.panes.contains_key(id))
        .or_else(|| {
            target.tab.and_then(|_| {
                ws.and_then(|w| w.tabs.get(tab_id))
                    .map(|tab| tab.layout.focus)
            })
        })
        .unwrap_or_else(|| app.layout().focus);
    let pane_cwd = app
        .panes
        .get(&focus)
        .map(|p| p.cwd.display().to_string())
        .unwrap_or_default();
    let (agent, status) = app
        .status
        .get(&focus)
        .map(|s| (s.agent.clone(), state_str(s.state).to_string()))
        .unwrap_or_default();

    json!({
        "workspace": {
            "id": ws_id.to_string(), "name": name.clone(),
            "cwd": ws_cwd.clone(), "branch": branch.clone(),
        },
        // Legacy alias for modules written against the old "node" key.
        "node": { "id": ws_id.to_string(), "name": name, "cwd": ws_cwd, "branch": branch },
        "tab": { "index": tab_index.to_string(), "name": tab_name },
        "pane": { "id": focus.0.to_string(), "cwd": pane_cwd, "agent": agent, "status": status },
        "selection": target.selection.clone().unwrap_or_default(),
        "invocation_source": source,
        "correlation_id": cid,
    })
}

/// The flat `LUVUS_*` vars mirroring the ids in `ctx`, so a shell script can use
/// them without parsing JSON. `LUVUS_PANE_ID` is only advisory here — for a
/// module *pane* luvus's own identity var always wins (see `Pane::build`).
pub fn env_from(ctx: &Value) -> Vec<(String, String)> {
    let s = |a: &str, b: &str| -> String {
        ctx.get(a)
            .and_then(|v| v.get(b))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    vec![
        ("LUVUS_WORKSPACE_ID".to_string(), s("workspace", "id")),
        ("LUVUS_WORKSPACE_CWD".to_string(), s("workspace", "cwd")),
        ("LUVUS_TAB_INDEX".to_string(), s("tab", "index")),
        ("LUVUS_PANE_ID".to_string(), s("pane", "id")),
        ("LUVUS_PANE_CWD".to_string(), s("pane", "cwd")),
        ("LUVUS_PANE_AGENT".to_string(), s("pane", "agent")),
        ("LUVUS_PANE_STATUS".to_string(), s("pane", "status")),
    ]
}

fn state_str(s: State) -> &'static str {
    match s {
        State::Blocked => "blocked",
        State::Working => "working",
        State::Done => "done",
        State::Idle => "idle",
        State::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_tab_target_builds_context_for_that_tab() {
        let _env = crate::persist::test_env("module-tab-context");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.workspaces[0].tabs[0].name = Some("first".into());
        app.run_cmd(crate::app::Cmd::NewTab);
        app.workspaces[0].tabs[1].name = Some("second".into());
        let second_pane = app.workspaces[0].tabs[1].layout.focus;
        app.workspaces[0].active_tab = 0;

        let context = build_for(&app, "menu:tab", &Target::tab(0, 1));
        assert_eq!(context["tab"]["index"], "2");
        assert_eq!(context["tab"]["name"], "second");
        assert_eq!(context["pane"]["id"], second_pane.0.to_string());
        assert_eq!(context["invocation_source"], "menu:tab");
        assert_eq!(
            app.workspaces[0].active_tab, 0,
            "building context is passive"
        );
    }
}
