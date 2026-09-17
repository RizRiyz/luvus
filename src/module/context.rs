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
        .or_else(|| (app.active_ws < app.workspaces.len()).then_some(app.active_ws))
        .or_else(|| (!app.workspaces.is_empty()).then_some(0));
    let ws = ws_id.and_then(|id| app.workspaces.get(id));
    let name = ws.map(|w| w.name.clone()).unwrap_or_default();
    let ws_cwd = ws.map(|w| w.cwd.display().to_string()).unwrap_or_default();
    let branch = ws.and_then(|w| w.branch.clone()).unwrap_or_default();
    let tab_id = ws.and_then(|w| {
        target
            .tab
            .filter(|index| *index < w.tabs.len())
            .or_else(|| (w.active_tab < w.tabs.len()).then_some(w.active_tab))
            .or_else(|| (!w.tabs.is_empty()).then_some(0))
    });
    let tab_index = tab_id
        .map(|index| (index + 1).to_string())
        .unwrap_or_default();
    let tab_name = ws
        .zip(tab_id)
        .and_then(|(w, index)| w.tabs.get(index))
        .and_then(|t| t.name.clone())
        .unwrap_or_default();

    // A targeted pane wins, but only while it still exists (a menu can outlive
    // its pane if the process exits between the right-click and the click).
    // During workspace creation a pane can exist briefly before its workspace
    // is appended; during shutdown no workspace or pane may exist at all. Keep
    // module context construction total across both transitions and never
    // manufacture a pane ID that was not present in the app.
    let focus = target
        .pane
        .filter(|id| app.panes.contains_key(id))
        .or_else(|| {
            target
                .tab
                .and_then(|_| ws.zip(tab_id))
                .and_then(|(w, index)| w.tabs.get(index))
                .map(|tab| tab.layout.focus)
                .filter(|id| app.panes.contains_key(id))
        })
        .or_else(|| {
            ws.zip(tab_id)
                .and_then(|(w, index)| w.tabs.get(index))
                .map(|tab| tab.layout.focus)
                .filter(|id| app.panes.contains_key(id))
        })
        .or_else(|| {
            app.workspaces
                .iter()
                .flat_map(|workspace| workspace.tabs.iter())
                .map(|tab| tab.layout.focus)
                .find(|id| app.panes.contains_key(id))
        })
        .or_else(|| app.panes.keys().copied().min_by_key(|id| id.0));
    let pane_cwd = focus
        .and_then(|id| app.panes.get(&id))
        .map(|p| p.cwd.display().to_string())
        .unwrap_or_default();
    let (agent, status) = focus
        .and_then(|id| app.status.get(&id))
        .map(|s| (s.agent.clone(), state_str(s.state).to_string()))
        .unwrap_or_default();
    let workspace_id = ws_id.map(|id| id.to_string()).unwrap_or_default();
    let pane_id = focus.map(|id| id.0.to_string()).unwrap_or_default();

    json!({
        "workspace": {
            "id": workspace_id.clone(), "name": name.clone(),
            "cwd": ws_cwd.clone(), "branch": branch.clone(),
        },
        // Legacy alias for modules written against the old "node" key.
        "node": { "id": workspace_id, "name": name, "cwd": ws_cwd, "branch": branch },
        "tab": { "index": tab_index, "name": tab_name },
        "pane": { "id": pane_id, "cwd": pane_cwd, "agent": agent, "status": status },
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

    #[test]
    fn empty_topology_builds_context_without_fake_ids() {
        let _env = crate::persist::test_env("module-empty-context");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.workspaces.clear();
        app.panes.clear();
        app.status.clear();
        app.active_ws = usize::MAX;

        let context = build(&app, "event");
        assert_eq!(context["workspace"]["id"], "");
        assert_eq!(context["tab"]["index"], "");
        assert_eq!(context["pane"]["id"], "");
    }

    #[test]
    fn stale_active_workspace_falls_back_to_surviving_topology() {
        let _env = crate::persist::test_env("module-stale-workspace-context");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.workspaces[0].tabs[0].layout.focus;
        app.active_ws = usize::MAX;

        let context = build(&app, "event");
        assert_eq!(context["workspace"]["id"], "0");
        assert_eq!(context["tab"]["index"], "1");
        assert_eq!(context["pane"]["id"], pane.0.to_string());
    }
}
