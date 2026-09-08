use super::super::*;
use crate::app::App;

pub(super) fn app(name: &str) -> (crate::persist::TestEnv, App) {
    let env = crate::persist::test_env(name);
    let (tx, _rx) = std::sync::mpsc::channel();
    let app = App::new(100, 40, tx).unwrap();
    (env, app)
}

pub(super) fn layout_state_bytes(app: &App) -> (PaneId, Vec<u8>, Vec<u8>) {
    let layout = app.layout();
    let tree = serde_json::to_vec(&layout.to_tree()).unwrap();
    let pane_sizes = serde_json::to_vec(
        &layout
            .panes(crate::api::topology::logical_area())
            .into_iter()
            .map(|pane| {
                json!({
                    "pane": pane.id.0,
                    "x": pane.rect.x,
                    "y": pane.rect.y,
                    "width": pane.rect.width,
                    "height": pane.rect.height,
                })
            })
            .collect::<Vec<_>>(),
    )
    .unwrap();
    (layout.focus, tree, pane_sizes)
}

pub(super) fn run_git(repo: &std::path::Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("git should run");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
