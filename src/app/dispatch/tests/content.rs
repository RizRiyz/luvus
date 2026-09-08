use super::super::*;
use super::support::*;
use crate::app::App;

#[test]
fn diff_api_validates_anchors_and_preserves_atomic_note_lifecycle() {
    let _env = crate::persist::test_env("diff-api");
    let repo = std::path::PathBuf::from(std::env::var_os("LUVUS_HOME").unwrap()).join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    run_git(&repo, &["init", "-q"]);
    run_git(&repo, &["config", "user.name", "Luvus Test"]);
    run_git(&repo, &["config", "user.email", "luvus@example.invalid"]);
    std::fs::write(repo.join("file.txt"), "old line\nstable\n").unwrap();
    run_git(&repo, &["add", "file.txt"]);
    run_git(&repo, &["commit", "-q", "-m", "base"]);
    std::fs::write(repo.join("file.txt"), "new line\nstable\n").unwrap();

    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(100, 30, tx).unwrap();
    app.workspaces[0].cwd = repo;

    let token = 1;
    app.diff.status_generation = token;
    let snapshot = crate::diff::git::scan(&app.workspaces[0].cwd, token).unwrap();
    assert!(app.apply_diff_status(token, app.workspaces[0].cwd.clone(), Ok(snapshot)));
    let refreshed = app.dispatch("diff.refresh", &json!({})).unwrap();
    assert_eq!(refreshed["refresh"], "complete");
    let listed = app
        .dispatch("diff.list", &json!({"layer":"worktree"}))
        .unwrap();
    assert_eq!(listed["files"].as_array().unwrap().len(), 1);
    assert_eq!(listed["files"][0]["path"], "file.txt");

    let loaded = app
        .dispatch(
            "diff.get",
            &json!({"path":"file.txt","layer":"worktree","include_patch":true}),
        )
        .unwrap();
    assert_eq!(loaded["additions"], 1);
    assert_eq!(loaded["deletions"], 1);
    assert!(loaded["hunks"][0]["lines"].is_array());

    let opened = app
        .dispatch(
            "diff.open",
            &json!({"path":"file.txt","layer":"worktree","placement":"tab","view":"stack"}),
        )
        .unwrap();
    let pane = opened["pane"].as_str().unwrap();
    assert!(app
        .dispatch("diff.navigate", &json!({"pane":pane,"action":"next_line"}))
        .is_ok());

    let invalid_state = app
        .dispatch("diff.note.list", &json!({"state":"unknown"}))
        .expect_err("unknown note states must not silently produce an empty list");
    assert_eq!(invalid_state.0, "diff_error");

    let empty_send = app
        .dispatch("diff.note.send", &json!({"to":"missing-agent","ids":[]}))
        .expect_err("empty review selection must fail before target resolution");
    assert_eq!(empty_send.0, "diff_error");
    assert_eq!(empty_send.1, "select at least one review note");

    let invalid_anchor = app
        .dispatch(
            "diff.note.add",
            &json!({"file":"file.txt","layer":"worktree","new_line":99,"body":"missing"}),
        )
        .expect_err("a note must reference a source line in the loaded diff");
    assert_eq!(invalid_anchor.0, "diff_error");
    assert!(app.diff.notes.is_empty());

    let added = app
        .dispatch(
            "diff.note.add",
            &json!({"file":"file.txt","layer":"worktree","new_line":1,"body":"check this"}),
        )
        .unwrap();
    let note_id = added["note"]["id"].as_str().unwrap().to_string();
    assert_eq!(app.diff.notes[0].anchor.context, "new line");
    assert_ne!(
        app.diff.notes[0].anchor.context_sha256,
        crate::diff::notes::context_hash("")
    );
    let open = app
        .dispatch("diff.note.list", &json!({"state":"open"}))
        .unwrap();
    assert_eq!(open["notes"].as_array().unwrap().len(), 1);

    let edited = app
        .dispatch("diff.note.edit", &json!({"id":note_id,"body":"updated"}))
        .unwrap();
    assert_eq!(edited["note"]["body"], "updated");
    let resolved = app
        .dispatch("diff.note.resolve", &json!({"id":note_id}))
        .unwrap();
    assert_eq!(resolved["note"]["state"], "resolved");
    let reopened = app
        .dispatch("diff.note.reopen", &json!({"id":note_id}))
        .unwrap();
    assert_eq!(reopened["note"]["state"], "open");
    app.dispatch("diff.note.remove", &json!({"id":note_id}))
        .unwrap();
    assert!(app.diff.notes.is_empty());

    let batch = app
        .dispatch(
            "diff.note.apply",
            &json!({"notes":[
                {"file":"file.txt","layer":"worktree","new_line":1,"body":"valid"},
                {"file":"file.txt","layer":"worktree","new_line":99,"body":"invalid"}
            ]}),
        )
        .expect_err("one invalid anchor must reject the whole batch");
    assert_eq!(batch.0, "diff_error");
    assert!(app.diff.notes.is_empty());
    assert!(crate::diff::notes::load(
        &app.diff.snapshot.as_ref().unwrap().repo_id,
        app.diff.loaded_review.as_ref().unwrap()
    )
    .unwrap()
    .is_empty());
}
