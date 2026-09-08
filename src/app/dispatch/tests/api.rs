use super::params::*;
use super::*;
use crate::app::App;

fn run_git(repo: &std::path::Path, args: &[&str]) {
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

#[test]
fn reported_usage_rejects_malformed_and_out_of_range_values() {
    let valid = json!({
        "model":"provider/model",
        "tokens_in":1,
        "tokens_out":2,
        "cache_read":3,
        "cache_write":4,
        "cost":0.5,
        "updated_at":100
    });
    let report = parse_reported_usage(&valid).unwrap();
    assert_eq!(report.value.cache, 7);

    for (field, value) in [
        ("tokens_in", json!(-1)),
        ("tokens_out", json!(1.5)),
        ("cache_read", json!(1_000_000_000_000_001_u64)),
        ("updated_at", json!(0)),
        ("updated_at", json!(9_007_199_254_740_992_u64)),
        ("cost", json!(-0.01)),
        ("cost", json!(1_000_000_000_001_f64)),
    ] {
        let mut malformed = valid.clone();
        malformed[field] = value;
        assert!(
            parse_reported_usage(&malformed).is_err(),
            "{field} was accepted: {malformed}"
        );
    }

    let mut control = valid.clone();
    control["model"] = json!("bad\nmodel");
    assert!(parse_reported_usage(&control).is_err());
    let mut unknown = valid;
    unknown["extra"] = json!(true);
    assert!(parse_reported_usage(&unknown).is_err());
}

#[test]
fn full_report_cache_allows_only_updates_and_same_pane_replacements() {
    let full = crate::mission::MAX_REPORTED_USAGE_ENTRIES;
    assert!(reported_usage_has_capacity(full, true, false));
    assert!(reported_usage_has_capacity(full, false, true));
    assert!(!reported_usage_has_capacity(full, false, false));
    assert!(reported_usage_has_capacity(full - 1, false, false));
}

#[test]
fn mission_open_targets_a_workspace_and_rejects_missing_ones() {
    let _env = crate::persist::test_env("mission-open-api");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(100, 30, tx).unwrap();
    let second =
        std::path::PathBuf::from(std::env::var_os("LUVUS_HOME").unwrap()).join("second-workspace");
    std::fs::create_dir_all(&second).unwrap();
    assert!(app.create_workspace_at(second));
    app.active_ws = 0;

    let opened = app
        .dispatch("mission.open", &json!({"workspace": "1"}))
        .expect("existing workspace opens Mission Control");
    assert_eq!(opened, json!({"type":"ok", "mission":true}));
    assert_eq!(app.active_ws, 1);
    assert!(app.active_is_mission());

    let before = (app.active_ws, app.ws().active_tab);
    let error = app
        .dispatch("mission.open", &json!({"workspace": "9"}))
        .expect_err("missing workspace must not change the active view");
    assert_eq!(error.0, "not_found");
    assert_eq!((app.active_ws, app.ws().active_tab), before);
}

#[test]
fn mission_snapshot_and_refresh_are_read_only_ui_independent_controls() {
    let _env = crate::persist::test_env("mission-snapshot-api");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(100, 30, tx).unwrap();
    let cwd = app.ws().cwd.clone();
    app.resumable.push(crate::agent::SessionInfo {
        agent: "codex".into(),
        session_id: "native-secret-id".into(),
        cwd,
        updated: std::time::SystemTime::now(),
    });
    app.agent_usage.insert(
        crate::mission::UsageKey::new("codex", "native-secret-id"),
        crate::mission::AgentUsage {
            model: "gpt-5".into(),
            tokens_in: 120,
            tokens_out: 30,
            cache: 10,
            context: Some(0.25),
            cost: Some(0.42),
        },
    );
    let before = (app.active_ws, app.ws().active_tab, app.active_is_mission());

    let snapshot = app
        .dispatch("mission.snapshot", &json!({"scope":"workspace"}))
        .unwrap();
    assert_eq!(snapshot["type"], "mission_snapshot");
    assert_eq!(snapshot["rows"][0]["kind"], "resumable");
    assert_eq!(snapshot["rows"][0]["usage"]["total_tokens"], 150);
    assert!(
        snapshot["rows"][0].get("session_id").is_none(),
        "read scope does not expose native session identifiers"
    );
    assert_eq!(
        (app.active_ws, app.ws().active_tab, app.active_is_mission()),
        before,
        "snapshot does not open or focus Mission Control"
    );

    let refreshed = app
        .dispatch("mission.refresh", &json!({"scope":"all"}))
        .unwrap();
    assert_eq!(refreshed["type"], "mission_refresh");
    assert_eq!(
        app.mission_usage_requested,
        Some(crate::mission::MissionUsageRequest {
            scope: crate::mission::MissionScope::All,
            workspace: 0,
        })
    );
    assert_eq!(
        (app.active_ws, app.ws().active_tab, app.active_is_mission()),
        before,
        "refresh queues work without changing the UI"
    );

    let bad_scope = app
        .dispatch("mission.snapshot", &json!({"scope":null}))
        .expect_err("a present non-string scope must not use the default");
    assert_eq!(bad_scope.0, "invalid_request");

    let all = app
        .dispatch("mission.snapshot", &json!({"scope":"all","workspace":999}))
        .expect("all-workspace scope does not depend on its anchor index");
    assert_eq!(all["type"], "mission_snapshot");
    assert_eq!(all["rows"][0]["kind"], "resumable");
}

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

#[test]
fn theme_api_lists_validates_and_applies_registry_entries() {
    let _env = crate::persist::test_env("theme-api");
    let source = crate::persist::ensure_config_dir().join("api-theme.toml");
    crate::theme::install::init(&source, "api-theme", Some("noir")).unwrap();
    crate::theme::install::install(source.to_str().unwrap(), true).unwrap();
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();

    let listed = app.dispatch("theme.list", &json!({})).unwrap();
    assert!(listed["themes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["id"] == "api-theme"));
    assert!(app
        .dispatch("theme.use", &json!({"id": "missing"}))
        .is_err());
    let selected = app
        .dispatch("theme.use", &json!({"id": "api-theme"}))
        .unwrap();
    assert_eq!(selected["id"], "api-theme");
    assert_eq!(app.config.theme, "api-theme");
}

#[test]
fn bar_api_validates_ownership_and_preserves_the_last_valid_widget() {
    let _env = crate::persist::test_env("bar-api");
    let module =
        std::path::PathBuf::from(std::env::var_os("LUVUS_HOME").unwrap()).join("bar-module");
    std::fs::create_dir_all(&module).unwrap();
    std::fs::write(
        module.join("luvus-module.toml"),
        r#"
id = "you.ci"
name = "CI"
version = "0.1.0"
min_luvus_version = "0.1.0"

[[bars]]
id = "status"
title = "CI status"
region = "top-right"
priority = 60

[[actions]]
id = "details"
title = "Details"
command = ["true"]
"#,
    )
    .unwrap();
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    app.module_link_with(&module, true, None).unwrap();

    let valid = json!({
        "owner": "you.ci",
        "id": "status",
        "content": [
            {"type":"text","text":"CI"},
            {"type":"state","state":"done","action":"details","value":"run-1"}
        ],
        "compact_content": [{"type":"state","state":"done"}]
    });
    let result = app.dispatch("ui.bar.push", &valid).unwrap();
    assert_eq!(result["changed"], true);
    let before = app.bar.widgets["you.ci:status"].clone();

    let mut invalid = valid;
    invalid["content"] = json!([{"type":"text","text":"\u{1b}[31mraw"}]);
    assert!(app.dispatch("ui.bar.push", &invalid).is_err());
    assert_eq!(app.bar.widgets["you.ci:status"], before);

    let mut wrong_action = invalid;
    wrong_action["content"] = json!([{"type":"text","text":"bad","action":"other-module-action"}]);
    assert!(app.dispatch("ui.bar.push", &wrong_action).is_err());
    assert_eq!(app.bar.widgets["you.ci:status"], before);

    app.dispatch(
        "ui.bar.move",
        &json!({"owner":"you.ci","id":"status","region":"bottom-right"}),
    )
    .unwrap();
    assert_eq!(
        app.config
            .bars
            .region_for("you.ci:status", crate::bar::BarRegion::TopRight),
        Some(crate::bar::BarRegion::BottomRight)
    );
    app.config.bars.bottom_right.push("other:widget".into());
    let order = app.config.bars.bottom_right.clone();
    app.dispatch(
        "ui.bar.move",
        &json!({"owner":"you.ci","id":"status","region":"bottom-right"}),
    )
    .unwrap();
    assert_eq!(
        app.config.bars.bottom_right, order,
        "an identical move must not rewrite or reorder persisted placement"
    );
    app.module_set_enabled("you.ci", false).unwrap();
    assert!(!app.bar.widgets.contains_key("you.ci:status"));
}

#[test]
fn unowned_notifications_share_the_same_rate_limit() {
    let _env = crate::persist::test_env("anonymous-notification-rate");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let request = json!({"text":"build finished"});

    for invalid in [
        json!({"text":"build finished","ttl_ms":0}),
        json!({"text":"bad\u{1b}content"}),
    ] {
        for _ in 0..40 {
            let error = app
                .dispatch("ui.notification.push", &invalid)
                .expect_err("invalid payloads must be rejected before rate limiting");
            assert_eq!(error.0, "invalid_request");
        }
    }
    for _ in 0..30 {
        app.dispatch("ui.notification.push", &request).unwrap();
    }
    let error = app
        .dispatch("ui.notification.push", &request)
        .expect_err("the shared anonymous bucket must be bounded");
    assert_eq!(error.0, "rate_limited");
}

#[test]
fn strip_title_icon_drops_a_leading_glyph_only() {
    // A leading spinner/status glyph and its space are removed.
    assert_eq!(
        strip_title_icon("✳ Ship the desktop release"),
        "Ship the desktop release"
    );
    assert_eq!(strip_title_icon("◐ Cogitating…"), "Cogitating…");
    assert_eq!(strip_title_icon("🤖  Opus 5"), "Opus 5");
    // No icon: unchanged apart from trimming.
    assert_eq!(strip_title_icon("  Ship it  "), "Ship it");
    assert_eq!(strip_title_icon("Ship it"), "Ship it");
    // ASCII punctuation and CJK letters are kept, not mistaken for an icon.
    assert_eq!(strip_title_icon("[WIP] fix bug"), "[WIP] fix bug");
    assert_eq!(strip_title_icon("実装 タスク"), "実装 タスク");
}

/// `ui.dock.push` carries a row's right-click menu (docs/52) through to the
/// stored `DockRow`, and a row that omits `menu` keeps the pre-existing
/// shape — that backward compatibility is the whole reason the field is
/// optional.
#[test]
fn dock_push_parses_a_rows_right_click_menu() {
    let _env = crate::persist::test_env("dock-push-menu");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();

    app.dispatch(
        "ui.dock.push",
        &json!({
            "id": "devices",
            "title": "DEVICES",
            "rows": [
                {"text": "esp32s3", "dot": "done",
                 "action": "select", "value": "/dev/ttyA",
                 "menu": [
                     {"title": "Flash this board", "action": "flash"},
                     {"title": "", "action": ""},
                     {"title": "Erase flash", "action": "erase", "destructive": true}
                 ]},
                {"text": "build", "action": "build"}
            ]
        }),
    )
    .expect("dock.push ok");

    let rows = &app.module_docks.get("devices").expect("dock stored").rows;
    assert_eq!(rows.len(), 2);

    let menu = &rows[0].menu;
    assert_eq!(menu.len(), 3);
    assert_eq!(menu[0].title, "Flash this board");
    assert_eq!(menu[0].action, "flash");
    assert!(!menu[0].destructive);
    assert!(menu[1].is_divider(), "an empty action is a divider");
    assert!(menu[2].destructive, "destructive survives the round trip");

    // No `menu` key at all: a row exactly as every earlier module pushes it.
    assert!(rows[1].menu.is_empty(), "absent menu stays absent");
    assert_eq!(rows[1].action.as_deref(), Some("build"));
}

/// A menu item may carry its **own** `value`, overriding the row's. That is
/// what lets one action back a menu of variants (`build` / `app` /
/// `bootloader`) without an action id per entry.
#[test]
fn dock_menu_item_value_overrides_the_rows_value() {
    let _env = crate::persist::test_env("dock-item-value");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();

    app.dispatch(
        "ui.dock.push",
        &json!({
            "id": "d",
            "rows": [{
                "text": "build", "action": "run", "value": "build",
                "menu": [
                    {"title": "App only",  "action": "run", "value": "app"},
                    {"title": "Erase",     "action": "run"}
                ]
            }]
        }),
    )
    .expect("push ok");

    let row = &app.module_docks.get("d").unwrap().rows[0];
    assert_eq!(row.menu[0].value.as_deref(), Some("app"));
    assert_eq!(row.menu[1].value, None, "no value falls back to the row's");

    // Resolution through the real click path is covered end-to-end by
    // `dock_menu_click_spawns_the_action_with_the_clicked_rows_env`.
}

/// `ui.dock.push` carries a row's `tone` and `spans` through to the stored
/// `DockRow`, a span without its own tone stays `None` so the renderer can
/// fall back to the row's, and a row that sends neither keeps the
/// pre-existing shape. The tone name is stored as sent: an unknown name is
/// resolved (and ignored) at draw time, never rejected here.
#[test]
fn dock_push_preserves_row_tone_and_spans() {
    let _env = crate::persist::test_env("dock-push-tone");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();

    app.dispatch(
        "ui.dock.push",
        &json!({
            "id": "quota",
            "rows": [
                {"text": "session 72%", "tone": "success"},
                {"text": "week [━━━───] 41%", "tone": "warning",
                 "spans": [
                     {"text": "week "},
                     {"text": "[", "tone": "muted"},
                     {"text": "━━━", "tone": "success"},
                     {"text": "───] 41%"}
                 ]},
                {"text": "plain"},
                {"text": "typo", "tone": "reddish"}
            ]
        }),
    )
    .expect("dock.push ok");

    let rows = &app.module_docks.get("quota").expect("dock stored").rows;
    assert_eq!(rows.len(), 4);

    assert_eq!(rows[0].tone.as_deref(), Some("success"));
    assert!(rows[0].spans.is_empty(), "no spans key stays empty");

    assert_eq!(rows[1].tone.as_deref(), Some("warning"));
    assert_eq!(
        rows[1].text, "week [━━━───] 41%",
        "text is kept beside spans"
    );
    let spans = &rows[1].spans;
    assert_eq!(spans.len(), 4);
    assert_eq!(spans[0].text, "week ");
    assert_eq!(
        spans[0].tone, None,
        "a span without a tone inherits at draw"
    );
    assert_eq!(spans[1].tone.as_deref(), Some("muted"));
    assert_eq!(spans[2].text, "━━━");
    assert_eq!(spans[2].tone.as_deref(), Some("success"));

    // Neither key: exactly what every earlier module pushes.
    assert_eq!(rows[2].tone, None);
    assert!(rows[2].spans.is_empty());

    // An unknown tone is stored verbatim; the draw path decides the fallback.
    assert_eq!(rows[3].tone.as_deref(), Some("reddish"));
}

/// Two easy module mistakes must not blank a row or hand its action an
/// empty target: a spans list of bare strings (or empty objects) parses
/// to no spans so the row falls back to `text`, and a spans-only row gets
/// `text` filled from its spans so `LUVUS_MODULE_ROW_TEXT` still names it.
#[test]
fn dock_push_drops_empty_spans_and_backfills_text_from_spans() {
    let _env = crate::persist::test_env("dock-push-span-edges");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();

    app.dispatch(
        "ui.dock.push",
        &json!({
            "id": "q",
            "rows": [
                {"text": "kept", "spans": ["not", "objects"]},
                {"text": "kept too", "spans": [{}, {"tone": "error"}, {"text": ""}]},
                {"spans": [{"text": "week "}, {"text": "41%", "tone": "warning"}]},
                {"text": "explicit", "spans": [{"text": "shown"}]}
            ]
        }),
    )
    .expect("push ok");

    let rows = &app.module_docks.get("q").unwrap().rows;
    assert!(rows[0].spans.is_empty(), "bare strings are not spans");
    assert_eq!(rows[0].text, "kept");
    assert!(
        rows[1].spans.is_empty(),
        "spans without text draw nothing, so drop them"
    );
    assert_eq!(rows[1].text, "kept too");
    assert_eq!(
        rows[2].text, "week 41%",
        "spans-only: text is the joined spans"
    );
    assert_eq!(rows[2].spans.len(), 2);
    assert_eq!(
        rows[3].text, "explicit",
        "an explicit text is never overwritten"
    );
}

/// External clients patch their rows from **both** `agent.list`
/// and `pane.agent_status_changed`. If the two disagree about what `project`
/// means, a renamed node visibly alternates between its label and its folder
/// basename as snapshots and events interleave. Pin the contract: both carry
/// the node label.
#[test]
fn agent_list_labels_a_pane_with_its_node_name() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    // Rename the node so its label and its cwd basename can't coincide.
    app.workspaces[0].name = "renamed-node".into();
    app.workspaces[0].branch = Some("feat/x".into());

    // Make the one existing pane look like a live agent.
    let pane = app.layout().focus;
    let s = app.status.get_mut(&pane).expect("pane has status");
    s.agent = "claude".into();
    s.state = State::Working;

    let out = app
        .dispatch("agent.list", &json!({}))
        .expect("agent.list ok");
    let row = &out["agents"][0];
    assert_eq!(row["agent"], "claude");
    assert_eq!(row["status"], "working");
    assert_eq!(row["workspace_id"], app.workspaces[0].id);
    assert_eq!(
        row["terminal_id"],
        app.panes
            .get(&pane)
            .and_then(|pane| pane.terminal_runtime())
            .map(|runtime| runtime.terminal_id)
            .expect("agent pane has a terminal lifetime")
    );
    // The label an API client renders, and the legacy field it falls back to.
    assert_eq!(row["project"], "renamed-node");
    assert_eq!(row["workspace_name"], "renamed-node");
    assert_eq!(row["branch"], "feat/x");
    // A plain node is not a linked worktree.
    assert_eq!(row["worktree"], false);
    // Nothing has reported a session for this pane, so it is explicitly
    // unbound rather than guessed — `agent.list` never invents one.
    assert!(row["session"].is_null(), "unbound session is null");

    // Once the integration hook reports one (or luvus launches it), the exact
    // id shows up here, which is how a script tells *which* conversation a
    // pane is running.
    app.status.get_mut(&pane).unwrap().agent_session = Some(crate::app::AgentSession {
        agent: "claude".into(),
        session_id: "sess-42".into(),
    });
    let out = app
        .dispatch("agent.list", &json!({}))
        .expect("agent.list ok");
    assert_eq!(out["agents"][0]["session"], "sess-42");
}

#[test]
fn pane_ids_are_checked_before_resolution_or_mutation() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    let overflow = u64::from(u32::MAX) + 1;
    let alias_one = u64::from(u32::MAX) + 2;

    for invalid in [
        json!(overflow),
        json!(alias_one),
        json!(-1),
        json!(1.5),
        json!(overflow.to_string()),
        json!("-1"),
        json!("1.5"),
        json!("abc"),
    ] {
        let error = app
            .resolve_pane(&json!({"pane": invalid}))
            .expect_err("malformed pane ids must be validation errors");
        assert_eq!(error.0, "invalid_request", "{invalid}");
    }
    assert_eq!(
        app.resolve_pane(&json!({"pane": pane.0})).unwrap(),
        Some(pane)
    );
    assert_eq!(
        app.resolve_pane(&json!({"pane": pane.0.to_string()}))
            .unwrap(),
        Some(pane)
    );
    assert_eq!(app.resolve_pane(&json!({})).unwrap(), Some(pane));
    assert_eq!(
        app.resolve_pane(&json!({"pane": null})).unwrap(),
        Some(pane)
    );
    let no_pane = app
        .orch_pane(&json!({"pane": null}))
        .expect_err("orchestration keeps explicit null as absent context");
    assert_eq!(no_pane.0, "no_pane");
    let missing = app
        .resolve_pane(&json!({"pane": u32::MAX}))
        .expect_err("a well-formed missing pane must be distinct from omission");
    assert_eq!(missing.0, "not_found");

    let leaves = app.layout().leaves();
    let focus = app.layout().focus;
    let revision = app.panes[&pane].content_revision();
    for (method, params) in [
        ("pane.close", json!({"pane": alias_one})),
        (
            "pane.send_input",
            json!({"pane": alias_one, "text": "exit\r"}),
        ),
    ] {
        let error = app
            .dispatch(method, &params)
            .expect_err("invalid pane ids must not mutate pane one");
        assert_eq!(error.0, "invalid_request");
        assert!(app.panes.contains_key(&pane));
        assert_eq!(app.layout().leaves(), leaves);
        assert_eq!(app.layout().focus, focus);
        assert_eq!(app.panes[&pane].content_revision(), revision);
    }

    let error = app
        .dispatch("agent.explain", &json!({"target": alias_one.to_string()}))
        .expect_err("an invalid explicit target must not fall back to focus");
    assert_eq!(error.0, "not_found");
    assert!(app.panes.contains_key(&pane));
}

/// `agent.report` and `agent.release` take their target only as `pane`, so an
/// explicit one that misses must be terminal rather than quietly acting on
/// the focused pane — the same rule `agent.explain` applies to `target`.
#[test]
fn explicit_agent_report_targets_never_fall_back_to_focus() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    let missing = (pane.0 + 4321).to_string();
    let report = json!({
        "pane": missing, "source": "hook", "agent": "claude", "status": "working",
    });
    let release = json!({ "pane": missing, "source": "hook" });

    for (method, params) in [
        ("agent.report", report.clone()),
        ("agent.release", release.clone()),
    ] {
        let error = app
            .dispatch(method, &params)
            .expect_err("a missing explicit pane is terminal");
        assert_eq!(error.0, "not_found", "{method}");
        assert!(
            app.status
                .get(&pane)
                .is_none_or(|s| s.agent_report.is_none()),
            "{method} must not have written authority to the focused pane"
        );
    }

    // `target` is not part of either signature, so it stays an unknown field
    // instead of opening a second resolution path.
    for (method, extra) in [("agent.report", report), ("agent.release", release)] {
        let mut params = extra;
        params["target"] = json!("reviewer");
        params.as_object_mut().unwrap().remove("pane");
        let error = app
            .dispatch(method, &params)
            .expect_err("target is not an accepted field");
        assert_eq!(error.0, "invalid_request", "{method}");
    }

    // A malformed explicit pane fails validation before any lookup.
    let error = app
        .dispatch(
            "agent.release",
            &json!({"pane": u64::from(u32::MAX) + 2, "source": "hook"}),
        )
        .expect_err("an out-of-range pane id must not wrap");
    assert_eq!(error.0, "invalid_request");
}

/// Every `agent.list` row carries its workspace's stable id. `workspace` is a
/// positional index that moves when workspaces are reordered or an earlier one
/// closes, and `workspace_name` is user-editable, so the id is the only
/// selector a consumer can hold across those changes.
#[test]
fn agent_list_rows_carry_a_stable_workspace_id() {
    let _env = crate::persist::test_env("agent-list-workspace-id");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();

    let first_pane = app.layout().focus;
    app.status.get_mut(&first_pane).unwrap().agent = "claude".into();
    let first_id = app.workspaces[0].id.clone();

    let second_root = crate::persist::config_dir().join("second-workspace");
    std::fs::create_dir_all(&second_root).unwrap();
    assert!(app.create_workspace_at(second_root));
    let second_pane = app.layout().focus;
    app.status.get_mut(&second_pane).unwrap().agent = "codex".into();
    let second_id = app.workspaces[1].id.clone();
    assert_ne!(first_id, second_id);

    let row_for = |app: &mut App, pane: crate::app::PaneId| -> Value {
        let out = app.dispatch("agent.list", &json!({})).expect("agent.list");
        let wanted = pane.0.to_string();
        out["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["pane"].as_str() == Some(wanted.as_str()))
            .expect("the agent is listed")
            .clone()
    };

    // The id agrees with the one `workspace.get` publishes for that index, so
    // a consumer can join the two surfaces without guessing.
    let second_row = row_for(&mut app, second_pane);
    assert_eq!(second_row["workspace"], "1");
    assert_eq!(second_row["workspace_id"], second_id.as_str());
    let published = app
        .dispatch("workspace.get", &json!({"workspace": 1}))
        .expect("workspace.get");
    assert_eq!(published["workspace_id"], second_row["workspace_id"]);

    // Reordering moves the index but not the id. This is the whole point of
    // the field: an index captured before the move now names the other
    // workspace, while the id still names this one.
    app.dispatch("workspace.move", &json!({"workspace": 1, "to": 0}))
        .expect("workspace.move");
    let moved = row_for(&mut app, second_pane);
    assert_eq!(
        moved["workspace"], "0",
        "the positional index followed the move"
    );
    assert_eq!(
        moved["workspace_id"],
        second_id.as_str(),
        "the stable id survived the move"
    );
    assert_eq!(
        row_for(&mut app, first_pane)["workspace_id"],
        first_id.as_str(),
        "the workspace that was displaced keeps its own id"
    );
}

/// A live alias set by `agent.name` shows up in `agent.list` and resolves an
/// `agent.*` target, and closing the pane prunes it.
#[test]
fn agent_name_aliases_a_pane_and_resolves_a_target() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    app.status.get_mut(&pane).unwrap().agent = "claude".into();

    // Name it, then it appears on the listing and resolves by name.
    app.dispatch(
        "agent.name",
        &json!({"pane": pane.0.to_string(), "name": "reviewer"}),
    )
    .expect("agent.name ok");
    let out = app.dispatch("agent.list", &json!({})).unwrap();
    assert_eq!(out["agents"][0]["name"], "reviewer");
    assert_eq!(
        app.resolve_agent_pane(&json!({"target": "reviewer"})),
        Some(pane)
    );
    // A numeric pane id resolves too.
    assert_eq!(
        app.resolve_agent_pane(&json!({"target": pane.0.to_string()})),
        Some(pane)
    );

    // An invalid grammar is refused.
    assert!(app
        .dispatch(
            "agent.name",
            &json!({"pane": pane.0.to_string(), "name": "Bad Name"})
        )
        .is_err());

    // Closing the pane drops the alias.
    app.close_pane(pane);
    assert!(app.agent_names.is_empty());
}

#[test]
fn agent_fork_api_targets_an_inactive_tab_and_can_preserve_focus() {
    let _env = crate::persist::test_env("agent-fork-api");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let source = app.layout().focus;
    {
        let status = app.status.get_mut(&source).unwrap();
        status.agent = "claude".into();
        status.agent_session = Some(AgentSession {
            agent: "claude".into(),
            session_id: "sess-api-fork".into(),
        });
    }
    app.set_agent_name(source, Some("reviewer"));

    // Leave the source in tab 1, then issue the request from tab 2. The
    // mutation must use the target's location without stealing UI focus.
    app.run_cmd(crate::app::keys::Cmd::NewTab);
    let active_pane = app.layout().focus;
    app.zoomed = true;
    let out = app
        .dispatch(
            "agent.fork",
            &json!({
                "target": "reviewer",
                "name": "experiment",
                "focus": false,
            }),
        )
        .expect("known Claude session forks");

    assert_eq!(out["type"], "agent_fork");
    assert_eq!(out["from"], source.0.to_string());
    assert_eq!(out["agent"], "claude");
    assert_eq!(out["name"], "experiment");
    assert_eq!(out["workspace"], "0");
    assert_eq!(out["tab"], "1");
    assert_eq!(out["focused"], false);
    let fork = PaneId(out["pane"].as_str().unwrap().parse().unwrap());
    assert_ne!(fork, source);
    assert_eq!(app.ws().active_tab, 1, "active tab was preserved");
    assert_eq!(app.layout().focus, active_pane, "active pane was preserved");
    assert!(app.zoomed, "--no-focus preserves the current zoom state");
    assert!(app.workspaces[0].tabs[0].layout.leaves().contains(&fork));
    assert_eq!(app.agent_names.get("experiment"), Some(&fork));
    assert_eq!(app.status.get(&fork).unwrap().agent, "claude");
}

#[test]
fn agent_fork_api_reports_validation_and_capability_errors() {
    let _env = crate::persist::test_env("agent-fork-api-errors");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    let target = pane.0.to_string();
    let before = app.panes.len();

    for params in [
        json!({"target": target, "focus": "no"}),
        json!({"target": target, "name": "Bad Name"}),
    ] {
        let err = app
            .dispatch("agent.fork", &params)
            .expect_err("invalid request must fail before spawning");
        assert_eq!(err.0, "invalid_request");
        assert_eq!(app.panes.len(), before);
    }

    let err = app
        .dispatch("agent.fork", &json!({"target": target}))
        .expect_err("a shell has no native agent fork");
    assert_eq!(err.0, "unsupported_agent");
    assert_eq!(app.panes.len(), before);

    let err = app
        .dispatch("agent.fork", &json!({"target": "missing"}))
        .expect_err("unknown targets are rejected");
    assert_eq!(err.0, "not_found");
}

#[test]
fn agent_send_requires_a_live_agent() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;

    // A plain shell is not an agent: send is refused as not-ready.
    let err = app
        .dispatch(
            "agent.send",
            &json!({"target": pane.0.to_string(), "text": "hi"}),
        )
        .expect_err("shell is not an agent");
    assert_eq!(err.0, "agent_not_ready");

    // Once detected as an agent, the send is accepted and echoes the pane.
    app.status.get_mut(&pane).unwrap().agent = "claude".into();
    let out = app
        .dispatch(
            "agent.send",
            &json!({"target": pane.0.to_string(), "text": "review"}),
        )
        .expect("agent.send ok");
    assert_eq!(out["pane"], pane.0.to_string());
    assert_eq!(out["agent"], "claude");

    // Empty text is refused; an unknown target is not found.
    assert!(app
        .dispatch(
            "agent.send",
            &json!({"target": pane.0.to_string(), "text": ""})
        )
        .is_err());
    assert_eq!(
        app.dispatch("agent.send", &json!({"target": "99999", "text": "x"}))
            .unwrap_err()
            .0,
        "not_found"
    );
}

#[test]
fn pane_input_methods_report_rejection_and_run_is_one_action() {
    let _env = crate::persist::test_env("pane-input-admission");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    let (tx, rx) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&pane)
        .unwrap()
        .replace_input_sender_for_test(tx);
    app.dispatch(
        "pane.run",
        &json!({"pane": pane.0.to_string(), "command": "echo hi"}),
    )
    .unwrap();
    let crate::terminal::pty::InputAction::Bytes(bytes) = rx.try_recv().unwrap() else {
        panic!("expected raw command")
    };
    assert_eq!(bytes, b"echo hi\r");
    assert!(rx.try_recv().is_err());
    app.panes[&pane]
        .engine
        .lock()
        .unwrap()
        .advance(b"\x1b[?2004h");
    app.dispatch(
        "pane.send_input",
        &json!({
            "pane": pane.0.to_string(),
            "text": "first\nsecond",
            "paste": true,
        }),
    )
    .unwrap();
    let crate::terminal::pty::InputAction::Bytes(bytes) = rx.try_recv().unwrap() else {
        panic!("expected bracketed paste")
    };
    assert_eq!(bytes, b"\x1b[200~first\nsecond\x1b[201~");
    let error = app
        .dispatch(
            "pane.send_input",
            &json!({
                "pane": pane.0.to_string(),
                "text": "must not be sent",
                "paste": "true",
            }),
        )
        .expect_err("a non-boolean paste value must be rejected");
    assert_eq!(error.0, "invalid_request");
    assert_eq!(error.1, "paste must be a boolean");
    assert!(rx.try_recv().is_err());
    drop(rx);
    for (method, params) in [
        (
            "pane.run",
            json!({"pane": pane.0.to_string(), "command": "echo hi"}),
        ),
        (
            "pane.send_input",
            json!({"pane": pane.0.to_string(), "text": "hi"}),
        ),
    ] {
        assert_eq!(app.dispatch(method, &params).unwrap_err().0, "send_failed");
    }
}

#[test]
fn agent_send_admits_one_ordered_submission_and_reports_closed_queue() {
    let _env = crate::persist::test_env("agent-send-atomic");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    app.status.get_mut(&pane).unwrap().agent = "claude".into();
    app.panes[&pane]
        .engine
        .lock()
        .unwrap()
        .advance(b"\x1b[?2004h");
    let (input_tx, input_rx) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&pane)
        .unwrap()
        .replace_input_sender_for_test(input_tx);
    for text in ["first\nsecond", "next"] {
        app.dispatch(
            "agent.send",
            &json!({"target": pane.0.to_string(), "text": text}),
        )
        .unwrap();
        let crate::terminal::pty::InputAction::Submit { paste, settle } =
            input_rx.try_recv().unwrap()
        else {
            panic!("paste and Enter must be a single action")
        };
        assert_eq!(paste, format!("\x1b[200~{text}\x1b[201~").as_bytes());
        assert_eq!(settle, std::time::Duration::from_millis(45));
        assert!(input_rx.try_recv().is_err());
    }
    drop(input_rx);
    let error = app
        .dispatch(
            "agent.send",
            &json!({"target": pane.0.to_string(), "text": "closed"}),
        )
        .unwrap_err();
    assert_eq!(error.0, "send_failed");
}

#[test]
fn atomic_agent_prompt_ignores_output_without_a_relevant_transition() {
    let _env = crate::persist::test_env("prompt-unrelated-output");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    app.status.get_mut(&pane).unwrap().agent = "codex".into();
    let (reply, response) = std::sync::mpsc::channel();
    let started = Instant::now();
    app.start_agent_prompt(
        "prompt-1".into(),
        json!({
            "target":pane.0.to_string(), "text":"review this", "wait":true,
            "until":["idle", "done", "blocked"], "timeout_s":10,
        }),
        reply,
        Arc::new(AtomicBool::new(false)),
    );
    assert!(
        response.try_recv().is_err(),
        "idle before evidence is not completion"
    );

    let revision = app.panes[&pane].content_revision_handle();
    app.panes[&pane]
        .engine
        .lock()
        .unwrap()
        .advance(b"\x1b]0;unrelated-title\x07");
    app.check_agent_waits(pane);
    revision.fetch_add(1, Ordering::Release);
    app.tick_agent_workflows(started + Duration::from_millis(10));
    app.tick_agent_workflows(started + Duration::from_millis(1220));
    assert!(
        response.try_recv().is_err(),
        "quiet output is not transition evidence"
    );
    app.status.get_mut(&pane).unwrap().state = State::Working;
    app.check_agent_waits(pane);
    app.status.get_mut(&pane).unwrap().state = State::Idle;
    app.check_agent_waits(pane);
    app.tick_agent_workflows(started + Duration::from_secs(2));
    let value: Value = serde_json::from_str(&response.recv().unwrap()).unwrap();
    assert_eq!(value["result"]["type"], "agent_prompt");
    assert_eq!(value["result"]["submitted"], true);
    assert_eq!(value["result"]["matched"], true);
    assert_eq!(value["result"]["evidence"], "state_transition");
    assert!(app.agent_prompts.is_empty());
}

#[test]
fn observed_prompt_pane_exit_is_a_structured_failure() {
    let _env = crate::persist::test_env("observed-prompt-exit");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    app.status.get_mut(&pane).unwrap().agent = "codex".into();
    let (reply, response) = std::sync::mpsc::channel();
    app.start_agent_prompt(
        "prompt".into(),
        json!({"target":pane.0.to_string(), "text":"review", "wait":true}),
        reply,
        Arc::new(AtomicBool::new(false)),
    );
    app.cancel_agent_waits(pane);
    let value: Value = serde_json::from_str(&response.try_recv().unwrap()).unwrap();
    assert_eq!(value["error"]["code"], "agent_not_running");
    assert_eq!(value["error"]["data"]["pane"], pane.0.to_string());
    assert_eq!(value["error"]["data"]["queued"], true);
    assert_eq!(value["error"]["data"]["submitted"], true);
    assert_eq!(value["error"]["data"]["observed_state"], Value::Null);
    assert_eq!(value["error"]["data"]["reason"], "pane_closed");
    assert!(value["error"]["data"]["baseline_revision"].is_u64());
    assert!(value["error"]["data"]["content_revision"].is_u64());
    assert!(app.agent_prompts.is_empty());
}

#[test]
fn observed_prompt_no_wait_keeps_the_queued_response_and_no_ownership() {
    let _env = crate::persist::test_env("prompt-no-wait");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    app.status.get_mut(&pane).unwrap().agent = "codex".into();
    let (input, received) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&pane)
        .unwrap()
        .replace_input_sender_for_test(input);
    for pending_wait in [false, true] {
        if pending_wait {
            let (reply, _response) = std::sync::mpsc::channel();
            app.start_agent_prompt(
                "waiting".into(),
                json!({"target":pane.0.to_string(),"text":"first","wait":true}),
                reply,
                Arc::new(AtomicBool::new(false)),
            );
            received.try_recv().unwrap();
        }
        let baseline = app.panes[&pane].content_revision();
        let (reply, response) = std::sync::mpsc::channel();
        app.start_agent_prompt(
            "queued".into(),
            json!({"target":pane.0.to_string(),"text":"review"}),
            reply,
            Arc::new(AtomicBool::new(false)),
        );
        let value: Value = serde_json::from_str(&response.try_recv().unwrap()).unwrap();
        assert_eq!(
            value,
            json!({"id":"queued","result":{
                "type":"agent_prompt","pane":pane.0.to_string(),"submitted":true,
                "matched":false,"status":"idle","baseline_revision":baseline,
                "content_revision":baseline,"evidence":"queued"
            }})
        );
        received.try_recv().unwrap();
        assert_eq!(app.agent_prompts.len(), usize::from(pending_wait));
    }
}

#[test]
fn observed_prompt_exited_terminal_releases_ownership_before_pane_removal() {
    let _env = crate::persist::test_env("prompt-terminal-exit");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    app.status.get_mut(&pane).unwrap().agent = "codex".into();
    let (reply, response) = std::sync::mpsc::channel();
    app.start_agent_prompt(
        "exit".into(),
        json!({"target":pane.0.to_string(),"text":"exit","wait":true}),
        reply,
        Arc::new(AtomicBool::new(false)),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !app.panes[&pane].child_exited() {
        assert!(Instant::now() < deadline, "test shell did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
    app.tick_agent_workflows(Instant::now());
    let value: Value = serde_json::from_str(&response.try_recv().unwrap()).unwrap();
    assert_eq!(value["error"]["code"], "agent_not_running");
    assert_eq!(value["error"]["data"]["pane"], pane.0.to_string());
    assert_eq!(value["error"]["data"]["reason"], "pane_closed");
    assert_eq!(value["error"]["data"]["observed_state"], Value::Null);
    assert!(app.agent_prompts.is_empty());
    assert_eq!(app.status[&pane].state, State::Idle);
}

#[test]
fn observed_prompt_requires_a_new_active_state_and_preserves_until() {
    let _env = crate::persist::test_env("observed-prompt-states");
    for initial in [State::Idle, State::Working, State::Blocked, State::Done] {
        for active in [State::Working, State::Blocked] {
            for until in [State::Idle, State::Working, State::Blocked, State::Done] {
                let (tx, _rx) = std::sync::mpsc::channel();
                let mut app = App::new(80, 24, tx).unwrap();
                let pane = app.layout().focus;
                let status = app.status.get_mut(&pane).unwrap();
                status.agent = "codex".into();
                status.state = initial;
                let (reply, response) = std::sync::mpsc::channel();
                app.start_agent_prompt("states".into(), json!({"target":pane.0.to_string(),"text":"review","wait":true,"until":[state_str(until)]}), reply, Arc::new(AtomicBool::new(false)));
                app.check_agent_waits(pane);
                app.tick_agent_workflows(Instant::now());
                assert!(
                    response.try_recv().is_err(),
                    "the initial state is not a new transition"
                );
                app.status.get_mut(&pane).unwrap().state = State::Idle;
                app.check_agent_waits(pane);
                app.status.get_mut(&pane).unwrap().state = active;
                app.check_agent_waits(pane);
                app.status.get_mut(&pane).unwrap().state = until;
                app.check_agent_waits(pane);
                app.tick_agent_workflows(Instant::now());
                let value: Value = serde_json::from_str(&response.try_recv().unwrap()).unwrap();
                assert_eq!(value["result"]["submitted"], true);
                assert_eq!(value["result"]["matched"], true);
                assert_eq!(value["result"]["status"], state_str(until));
                assert_eq!(value["result"]["observed_state"], state_str(active));
                assert!(app.agent_prompts.is_empty());
            }
        }
    }
}

#[test]
fn observed_prompt_unknown_state_times_out_without_changing_status() {
    let _env = crate::persist::test_env("observed-prompt-unknown");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    let status = app.status.get_mut(&pane).unwrap();
    status.agent = "codex".into();
    status.state = State::Unknown;
    let (reply, response) = std::sync::mpsc::channel();
    app.start_agent_prompt(
        "prompt".into(),
        json!({"target":pane.0.to_string(),"text":"review","wait":true,"timeout_s":0}),
        reply,
        Arc::new(AtomicBool::new(false)),
    );
    app.tick_agent_workflows(Instant::now());
    let value: Value = serde_json::from_str(&response.try_recv().unwrap()).unwrap();
    assert_eq!(value["result"]["evidence"], "timeout");
    assert_eq!(value["result"]["matched"], false);
    assert_eq!(value["result"]["observed_state"], Value::Null);
    assert_eq!(app.status[&pane].state, State::Unknown);
    assert!(app.agent_prompts.is_empty());
}

#[test]
fn observed_prompt_cancellation_and_missing_terminal_release_ownership() {
    let _env = crate::persist::test_env("observed-prompt-cleanup");
    for cancel in [true, false] {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        app.status.get_mut(&pane).unwrap().agent = "codex".into();
        let (reply, response) = std::sync::mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        app.start_agent_prompt(
            "prompt".into(),
            json!({"target":pane.0.to_string(),"text":"review","wait":true}),
            reply,
            cancelled.clone(),
        );
        if cancel {
            cancelled.store(true, Ordering::Release);
        } else {
            app.panes.remove(&pane);
        }
        app.tick_agent_workflows(Instant::now());
        assert!(app.agent_prompts.is_empty());
        if cancel {
            assert!(response.try_recv().is_err());
        } else {
            let value: Value = serde_json::from_str(&response.try_recv().unwrap()).unwrap();
            assert_eq!(value["error"]["code"], "agent_not_running");
            assert_eq!(value["error"]["data"]["submitted"], true);
        }
    }
}

#[test]
fn observed_prompt_rejected_requests_never_queue_input() {
    let _env = crate::persist::test_env("observed-prompt-rejections");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    app.status.get_mut(&pane).unwrap().agent = "codex".into();
    let (input, received) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&pane)
        .unwrap()
        .replace_input_sender_for_test(input);
    for patch in [
        json!({"extra":true}),
        json!({"target":"missing"}),
        json!({"text":""}),
        json!({"text":7}),
        json!({"text":"x".repeat(MAX_AGENT_PROMPT_CHARS + 1)}),
        json!({"wait":"true"}),
        json!({"until":["done"]}),
        json!({"timeout_s":1}),
        json!({"wait":true,"until":[]}),
        json!({"wait":true,"until":["unknown"]}),
        json!({"wait":true,"timeout_s":-1}),
        json!({"wait":true,"timeout_s":3601}),
        json!({"wait":true,"timeout_s":"1"}),
    ] {
        let mut params = json!({"target":pane.0.to_string(),"text":"review"});
        params
            .as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        let (reply, response) = std::sync::mpsc::channel();
        app.start_agent_prompt(
            "invalid".into(),
            params,
            reply,
            Arc::new(AtomicBool::new(false)),
        );
        let value: Value = serde_json::from_str(&response.try_recv().unwrap()).unwrap();
        assert!(value.get("error").is_some());
        assert!(received.try_recv().is_err());
        assert!(app.agent_prompts.is_empty());
    }
    let (reply, response) = std::sync::mpsc::channel();
    app.start_agent_prompt(
        "cancelled".into(),
        json!({"target":pane.0.to_string(),"text":"review"}),
        reply,
        Arc::new(AtomicBool::new(true)),
    );
    assert!(response.try_recv().is_err());
    assert!(received.try_recv().is_err());
    assert!(app.agent_prompts.is_empty());
    drop(received);
    let (reply, response) = std::sync::mpsc::channel();
    app.start_agent_prompt(
        "send-failed".into(),
        json!({"target":pane.0.to_string(),"text":"review"}),
        reply,
        Arc::new(AtomicBool::new(false)),
    );
    let value: Value = serde_json::from_str(&response.try_recv().unwrap()).unwrap();
    assert_eq!(value["error"]["code"], "send_failed");
    assert!(app.agent_prompts.is_empty());
}

#[test]
fn observed_prompt_admission_failures_preserve_input_and_ownership() {
    let _env = crate::persist::test_env("observed-prompt-admission");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    let (input, received) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&pane)
        .unwrap()
        .replace_input_sender_for_test(input);
    let params = json!({"target":pane.0.to_string(),"text":"review","wait":true});
    let (reply, response) = std::sync::mpsc::channel();
    app.start_agent_prompt(
        "shell".into(),
        params.clone(),
        reply,
        Arc::new(AtomicBool::new(false)),
    );
    let value: Value = serde_json::from_str(&response.try_recv().unwrap()).unwrap();
    assert_eq!(value["error"]["code"], "agent_not_ready");
    assert!(received.try_recv().is_err());
    assert!(app.agent_prompts.is_empty());
    app.status.get_mut(&pane).unwrap().agent = "codex".into();
    let (reply, _response) = std::sync::mpsc::channel();
    app.start_agent_prompt(
        "first".into(),
        params.clone(),
        reply.clone(),
        Arc::new(AtomicBool::new(false)),
    );
    let _queued = received.try_recv().unwrap();
    let (second_reply, second_response) = std::sync::mpsc::channel();
    app.start_agent_prompt(
        "busy".into(),
        params.clone(),
        second_reply,
        Arc::new(AtomicBool::new(false)),
    );
    let value: Value = serde_json::from_str(&second_response.try_recv().unwrap()).unwrap();
    assert_eq!(value["error"]["code"], "agent_prompt_busy");
    assert!(received.try_recv().is_err());
    assert_eq!(app.agent_prompts[&pane].len(), 1);
    for _ in 1..MAX_AGENT_WAITS_TOTAL {
        app.agent_prompts.get_mut(&pane).unwrap().push(AgentPrompt {
            request_id: "capacity-fixture".into(),
            until: vec![State::Done],
            baseline_revision: 0,
            last_revision: 0,
            last_state: Some(State::Idle),
            observed_state: None,
            reply: reply.clone(),
            deadline: Instant::now() + MAX_AGENT_WAIT,
            cancelled: Arc::new(AtomicBool::new(false)),
        });
    }
    let (reply, response) = std::sync::mpsc::channel();
    app.start_agent_prompt(
        "full".into(),
        params,
        reply,
        Arc::new(AtomicBool::new(false)),
    );
    let value: Value = serde_json::from_str(&response.try_recv().unwrap()).unwrap();
    assert_eq!(value["error"]["code"], "unavailable");
    assert!(received.try_recv().is_err());
    assert_eq!(app.agent_prompts[&pane].len(), MAX_AGENT_WAITS_TOTAL);
}

#[test]
fn observed_prompt_completion_timeout_preserves_transition_evidence() {
    let _env = crate::persist::test_env("observed-prompt-deadline");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    app.status.get_mut(&pane).unwrap().agent = "codex".into();
    let (reply, response) = std::sync::mpsc::channel();
    app.start_agent_prompt("prompt".into(), json!({"target":pane.0.to_string(),"text":"review","wait":true,"until":["done"],"timeout_s":1}), reply, Arc::new(AtomicBool::new(false)));
    app.status.get_mut(&pane).unwrap().state = State::Working;
    app.check_agent_waits(pane);
    app.status.get_mut(&pane).unwrap().state = State::Unknown;
    app.tick_agent_workflows(Instant::now() + Duration::from_secs(2));
    let value: Value = serde_json::from_str(&response.try_recv().unwrap()).unwrap();
    assert_eq!(value["result"]["submitted"], true);
    assert_eq!(value["result"]["matched"], false);
    assert_eq!(value["result"]["observed_state"], "working");
    assert_eq!(value["result"]["evidence"], "timeout");
    assert_eq!(value["result"]["status"], "unknown");
    assert!(app.agent_prompts.is_empty());
}

#[test]
fn atomic_agent_prompt_reports_queued_timeout_without_resubmitting() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    app.status.get_mut(&pane).unwrap().agent = "codex".into();
    let (reply, response) = std::sync::mpsc::channel();
    app.start_agent_prompt(
        "prompt-timeout".into(),
        json!({"target":pane.0.to_string(), "text":"review", "wait":true, "timeout_s":0}),
        reply,
        Arc::new(AtomicBool::new(false)),
    );
    app.status.get_mut(&pane).unwrap().state = State::Working;
    app.check_agent_waits(pane);
    app.tick_agent_workflows(Instant::now());
    let value: Value = serde_json::from_str(&response.recv().unwrap()).unwrap();
    assert_eq!(value["result"]["submitted"], true);
    assert_eq!(value["result"]["matched"], false);
    assert_eq!(value["result"]["evidence"], "timeout");
    assert_eq!(value["result"]["observed_state"], Value::Null);
    assert!(app.agent_prompts.is_empty());
}

#[test]
fn atomic_agent_prompt_rejects_an_overlapping_wait_before_queueing() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    app.status.get_mut(&pane).unwrap().agent = "codex".into();
    let (first_reply, _first_response) = std::sync::mpsc::channel();
    app.start_agent_prompt(
        "prompt-first".into(),
        json!({"target":pane.0.to_string(), "text":"first", "wait":true}),
        first_reply,
        Arc::new(AtomicBool::new(false)),
    );

    let (second_reply, second_response) = std::sync::mpsc::channel();
    app.start_agent_prompt(
        "prompt-second".into(),
        json!({"target":pane.0.to_string(), "text":"second", "wait":true}),
        second_reply,
        Arc::new(AtomicBool::new(false)),
    );
    let value: Value = serde_json::from_str(&second_response.recv().unwrap()).unwrap();
    assert_eq!(value["error"]["code"], "agent_prompt_busy");
    assert_eq!(app.agent_prompts[&pane].len(), 1);
}

#[test]
fn server_owned_agent_start_reserves_name_and_waits_for_detection() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    let (reply, response) = std::sync::mpsc::channel();
    app.start_agent_launch(
        "start-1".into(),
        json!({
            "name":"reviewer", "kind":"codex", "pane":pane.0.to_string(),
            "args":[], "timeout_s":10,
        }),
        reply,
        Arc::new(AtomicBool::new(false)),
    );
    assert_eq!(app.agent_names.get("reviewer"), Some(&pane));
    assert!(response.try_recv().is_err());

    let status = app.status.get_mut(&pane).unwrap();
    status.agent = "codex".into();
    status.state = State::Working;
    app.tick_agent_workflows(Instant::now());
    let value: Value = serde_json::from_str(&response.recv().unwrap()).unwrap();
    assert_eq!(value["result"]["type"], "agent_start");
    assert_eq!(value["result"]["ready"], true);
    assert_eq!(value["result"]["name"], "reviewer");
    assert_eq!(value["result"]["status"], "working");
}

#[test]
fn server_owned_agent_start_keeps_null_targets_terminal() {
    for params in [
        json!({
            "name":"reviewer", "kind":"codex", "pane":null,
            "args":[], "timeout_s":10,
        }),
        json!({
            "name":"reviewer", "kind":"codex", "anchor":null,
            "args":[], "timeout_s":10,
        }),
    ] {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let before_focus = app.layout().focus;
        let before_leaves = app.layout().leaves();
        let before_panes = app.panes.len();
        let (reply, response) = std::sync::mpsc::channel();

        app.start_agent_launch(
            "start-null".into(),
            params,
            reply,
            Arc::new(AtomicBool::new(false)),
        );

        let value: Value = serde_json::from_str(&response.recv().unwrap()).unwrap();
        assert_eq!(value["error"]["code"], "not_found");
        assert!(app.agent_names.is_empty());
        assert!(app.agent_starts.is_empty());
        assert_eq!(app.layout().focus, before_focus);
        assert_eq!(app.layout().leaves(), before_leaves);
        assert_eq!(app.panes.len(), before_panes);
    }
}

#[test]
fn integration_report_is_explainable_exclusive_and_resolves_waits() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    let (reply, response) = std::sync::mpsc::channel();
    app.register_agent_wait(
        pane,
        "wait-1".into(),
        vec![State::Blocked],
        reply,
        Some(Duration::from_secs(1)),
        Arc::new(AtomicBool::new(false)),
    );

    let reported = app
        .dispatch(
            "agent.report",
            &json!({
                "pane":pane.0.to_string(), "source":"fx/plugin", "agent":"newagent",
                "status":"blocked", "message":"approval required", "sequence":7,
                "ttl_s":60,
            }),
        )
        .unwrap();
    assert_eq!(reported["status"], "blocked");
    let waited: Value = serde_json::from_str(&response.recv().unwrap()).unwrap();
    assert_eq!(waited["result"]["matched"], true);
    assert_eq!(waited["result"]["status"], "blocked");

    let explanation = app
        .dispatch("agent.explain", &json!({"target":pane.0.to_string()}))
        .unwrap();
    assert_eq!(explanation["identity"]["source"], "integration_report");
    assert_eq!(explanation["identity"]["confidence"], "authoritative");
    assert_eq!(
        explanation["state_evidence"]["blocked_hint"],
        "approval required"
    );
    assert!(
        app.is_agent_pane(pane),
        "a reported new agent is immediately live"
    );

    assert_eq!(
        app.dispatch(
            "agent.report",
            &json!({"pane":pane.0.to_string(), "source":"other", "agent":"newagent", "status":"idle"}),
        )
        .unwrap_err()
        .0,
        "authority_conflict"
    );
    assert_eq!(
        app.dispatch(
            "agent.report",
            &json!({"pane":pane.0.to_string(), "source":"fx/plugin", "agent":"newagent", "status":"idle", "sequence":7}),
        )
        .unwrap_err()
        .0,
        "stale_report"
    );
    app.dispatch(
        "agent.release",
        &json!({"pane":pane.0.to_string(), "source":"fx/plugin"}),
    )
    .unwrap();
    assert!(app.status[&pane].agent_report.is_none());
    assert!(app.status[&pane].force_detect);
}

#[test]
fn runtime_snapshot_is_global_fenced_and_processes_hide_arguments() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    app.proc_commands.insert(
        pane,
        vec![
            "/bin/zsh -l".into(),
            "/usr/bin/node /tools/codex.js --token secret-value".into(),
        ],
    );
    let processes = app
        .dispatch("pane.processes", &json!({"pane":pane.0}))
        .unwrap();
    assert_eq!(processes["executables"], json!(["zsh", "node", "codex.js"]));
    assert_eq!(processes["arguments_exposed"], false);
    assert!(!processes.to_string().contains("secret-value"));

    let snapshot = app.dispatch("session.snapshot", &json!({})).unwrap();
    assert_eq!(snapshot["type"], "session_snapshot");
    assert_eq!(snapshot["protocol"]["name"], "luvus-uhp");
    assert_eq!(
        snapshot["workspaces"][0]["tabs"][0]["panes"][0]["pane_id"],
        pane.0.to_string()
    );
    assert!(snapshot["event_sequence"].is_u64());
}

#[test]
fn a_target_resolves_by_kind_when_unique_and_is_ambiguous_when_not() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let a = app.layout().focus;
    app.status.get_mut(&a).unwrap().agent = "claude".into();

    // One claude: the kind resolves it directly.
    assert_eq!(
        app.resolve_agent_target(&json!({"target": "claude"})),
        Ok(a)
    );

    // A second claude in a new pane makes the kind ambiguous.
    app.split(crate::layout::Axis::Col);
    let b = app.layout().focus;
    app.status.get_mut(&b).unwrap().agent = "claude".into();
    let err = app
        .resolve_agent_target(&json!({"target": "claude"}))
        .expect_err("two claudes are ambiguous");
    assert_eq!(err.0, "ambiguous_target");

    // A name still disambiguates.
    app.agent_names.insert("web".into(), b);
    assert_eq!(app.resolve_agent_target(&json!({"target": "web"})), Ok(b));
    // And a kind with no live agent is simply not found.
    assert_eq!(
        app.resolve_agent_target(&json!({"target": "codex"}))
            .unwrap_err()
            .0,
        "not_found"
    );
}

#[test]
fn agent_keys_requires_a_recognized_agent_before_sending() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    let t = pane.0.to_string();
    let (input_tx, input_rx) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&pane)
        .unwrap()
        .replace_input_sender_for_test(input_tx);

    let error = app
        .dispatch("agent.keys", &json!({"target": t, "keys": ["enter"]}))
        .expect_err("plain shells are not agent targets");
    assert_eq!(error.0, "agent_not_ready");
    assert!(input_rx.try_recv().is_err());
}

#[test]
fn agent_keys_validates_the_entire_array_before_sending() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    app.status.get_mut(&pane).unwrap().agent = "claude".into();
    let t = pane.0.to_string();
    let (input_tx, input_rx) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&pane)
        .unwrap()
        .replace_input_sender_for_test(input_tx);

    for keys in [
        json!([]),
        Value::Null,
        json!("enter"),
        json!(["enter", 7]),
        json!(["enter", "not-a-key"]),
    ] {
        let error = app
            .dispatch("agent.keys", &json!({"target": t, "keys": keys}))
            .expect_err("invalid arrays must fail atomically");
        assert_eq!(error.0, "invalid_request");
        assert!(input_rx.try_recv().is_err(), "no prefix may be queued");
    }
    let error = app
        .dispatch(
            "agent.keys",
            &json!({"target": t, "keys": ["enter"], "extra": true}),
        )
        .expect_err("unknown fields must fail before delivery");
    assert_eq!(error.0, "invalid_request");
    assert!(input_rx.try_recv().is_err());
}

#[test]
fn agent_keys_queues_valid_bytes_once_in_request_order() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    app.status.get_mut(&pane).unwrap().agent = "claude".into();
    let t = pane.0.to_string();
    let (input_tx, input_rx) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&pane)
        .unwrap()
        .replace_input_sender_for_test(input_tx);

    app.dispatch(
        "agent.keys",
        &json!({"target": t, "keys": ["up", "enter", "ctrl+c"]}),
    )
    .expect("known keys queue");
    let crate::terminal::pty::InputAction::Bytes(bytes) = input_rx.recv().unwrap() else {
        panic!("agent.keys must enqueue bytes")
    };
    assert_eq!(bytes, b"\x1b[A\r\x03");
    assert!(input_rx.try_recv().is_err(), "the batch is one queue item");
}

#[test]
fn agent_keys_reports_a_closed_writer() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    app.status.get_mut(&pane).unwrap().agent = "claude".into();
    let t = pane.0.to_string();
    let (input_tx, input_rx) = std::sync::mpsc::channel();
    drop(input_rx);
    app.panes
        .get_mut(&pane)
        .unwrap()
        .replace_input_sender_for_test(input_tx);

    let error = app
        .dispatch(
            "agent.keys",
            &json!({"target": t, "keys": ["enter", "esc"]}),
        )
        .expect_err("a closed input queue is a delivery failure");
    assert_eq!(error.0, "send_failed");
}

#[test]
fn key_names_map_to_terminal_bytes() {
    for name in [
        "enter",
        "ENTER",
        "return",
        "cr",
        "esc",
        "escape",
        "tab",
        "space",
        "backspace",
        "bs",
        "delete",
        "del",
        "up",
        "down",
        "right",
        "left",
        "home",
        "end",
        "pageup",
        "pgup",
        "pagedown",
        "pgdn",
        "a",
        "é",
        "🙂",
    ] {
        assert!(key_to_bytes(name).is_some(), "previously valid key {name}");
    }
    assert_eq!(key_to_bytes("ctrl+c").as_deref(), Some(&[0x03u8][..]));
    assert_eq!(key_to_bytes("CTRL+Z").as_deref(), Some(&[0x1au8][..]));
    assert_eq!(key_to_bytes("C-d").as_deref(), Some(&[0x04u8][..]));
    assert!(key_to_bytes("f13").is_none());
    assert!(key_to_bytes("ctrl+1").is_none());
    assert!(key_to_bytes("\n").is_none());
}

#[test]
fn pane_rename_modal_sets_and_clears_the_name() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent};
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;

    app.open_pane_rename(pane);
    for c in "worker".chars() {
        app.handle_pane_rename_key(KeyEvent::from(KeyCode::Char(c)));
    }
    app.handle_pane_rename_key(KeyEvent::from(KeyCode::Enter));
    assert_eq!(app.agent_name_for(pane), Some("worker"));
    assert!(app.pane_rename.is_none());

    // Reopen pre-filled, then clear by emptying and committing.
    app.open_pane_rename(pane);
    assert_eq!(app.pane_rename.as_ref().unwrap().buffer, "worker");
    for _ in 0..6 {
        app.handle_pane_rename_key(KeyEvent::from(KeyCode::Backspace));
    }
    app.handle_pane_rename_key(KeyEvent::from(KeyCode::Enter));
    assert_eq!(app.agent_name_for(pane), None);
}

#[test]
fn pane_rename_does_not_turn_backend_label_into_alias() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    app.backend_labels.insert(pane, "harness-shell".into());

    app.open_pane_rename(pane);
    assert_eq!(app.pane_rename.as_ref().unwrap().buffer, "");
    assert!(app.agent_names.values().all(|target| *target != pane));
    assert_eq!(app.agent_name_for(pane), Some("harness-shell"));
}

/// An explicit null pane has the same focused-pane semantics as omission.
#[test]
fn pane_split_null_pane_targets_the_focused_layout_pane() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let base = app.layout().focus;

    let out = app
        .dispatch("pane.split", &json!({"pane": null, "focus": false}))
        .expect("an explicit null pane falls back to layout focus");
    let split = PaneId(out["pane"].as_str().unwrap().parse().unwrap());

    assert_ne!(split, base);
    assert_eq!(out["workspace"], "0");
    assert_eq!(out["tab"], "1");
    assert_eq!(app.pane_location(split), Some((0, 0)));
    assert_eq!(app.layout().focus, base);
}

/// Background and default splits preserve their established focus behavior.
#[test]
fn pane_split_no_focus_keeps_the_caller_focused() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let base = app.layout().focus;

    // Background split: a new pane appears, but focus stays on the caller.
    let out = app
        .dispatch("pane.split", &json!({"focus": false}))
        .unwrap();
    assert_ne!(out["pane"], base.0.to_string());
    assert_eq!(app.layout().focus, base);

    // Default split still moves focus to the new pane.
    let out2 = app.dispatch("pane.split", &json!({})).unwrap();
    assert_eq!(app.layout().focus.0.to_string(), out2["pane"]);
}

/// Cross-workspace splits attach once and report their owning workspace and tab.
#[test]
fn pane_split_targets_foreign_workspace_without_detaching() {
    let _env = crate::persist::test_env("pane-split-foreign-workspace");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let caller_ws = app.active_ws;
    let caller_tab = app.workspaces[caller_ws].active_tab;
    let caller_pane = app.layout().focus;

    let target_root = crate::persist::config_dir().join("target-workspace");
    let target_cwd = target_root.join("nested-pane-cwd");
    std::fs::create_dir_all(&target_cwd).unwrap();
    assert!(app.create_workspace_at(target_root.clone()));
    let target_ws = app.active_ws;
    let target_tab = app.workspaces[target_ws].active_tab;
    let target_pane = app.layout().focus;
    app.panes.get_mut(&target_pane).unwrap().cwd = target_cwd.clone();

    app.active_ws = caller_ws;
    app.workspaces[caller_ws].active_tab = caller_tab;
    app.workspaces[caller_ws].tabs[caller_tab].layout.focus = caller_pane;
    app.zoomed = true;

    let out = app
        .dispatch(
            "pane.split",
            &json!({"pane": target_pane.0.to_string(), "focus": false}),
        )
        .unwrap();
    let new_pane = PaneId(out["pane"].as_str().unwrap().parse().unwrap());

    assert_eq!(out["workspace"], target_ws.to_string());
    assert_eq!(out["tab"], (target_tab + 1).to_string());
    assert_eq!(
        app.active_ws, caller_ws,
        "the caller's workspace stays active"
    );
    assert_eq!(app.workspaces[caller_ws].active_tab, caller_tab);
    assert_eq!(app.layout().focus, caller_pane, "the caller keeps focus");
    assert!(app.zoomed, "a background split preserves caller zoom");
    assert_eq!(app.pane_location(new_pane), Some((target_ws, target_tab)));
    assert_eq!(
        app.workspaces[target_ws].tabs[target_tab].layout.focus, target_pane,
        "the inactive target tab's focus is restored"
    );
    assert_eq!(
        app.panes.get(&new_pane).map(|pane| &pane.cwd),
        Some(&target_cwd),
        "a split inherits the target pane's live cwd"
    );
    let occurrences = app
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.tabs)
        .flat_map(|tab| tab.layout.leaves())
        .filter(|pane| *pane == new_pane)
        .count();
    assert_eq!(occurrences, 1, "every spawned pane has one layout owner");

    app.status.get_mut(&new_pane).unwrap().agent = "claude".into();
    let agents = app.dispatch("agent.list", &json!({})).unwrap();
    let new_pane_text = new_pane.0.to_string();
    let row = agents["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["pane"].as_str() == Some(new_pane_text.as_str()))
        .expect("the attached pane is visible to agent.list");
    assert_eq!(row["workspace"], target_ws.to_string());
    assert_eq!(row["tab"], (target_tab + 1).to_string());

    app.config.layout.new_pane_to_workspace_root = true;
    let root_out = app
        .dispatch(
            "pane.split",
            &json!({"pane": target_pane.0.to_string(), "focus": false}),
        )
        .unwrap();
    let root_pane = PaneId(root_out["pane"].as_str().unwrap().parse().unwrap());
    assert_eq!(app.pane_location(root_pane), Some((target_ws, target_tab)));
    assert_eq!(
        app.panes.get(&root_pane).map(|pane| &pane.cwd),
        Some(&target_root),
        "root-first mode uses the target workspace root, not the caller's"
    );

    app.config.layout.new_pane_to_workspace_root = false;
    app.zoomed = true;
    app.scroll_pane = Some(caller_pane);
    let focused_out = app
        .dispatch("pane.split", &json!({"pane": target_pane.0.to_string()}))
        .unwrap();
    let focused_pane = PaneId(focused_out["pane"].as_str().unwrap().parse().unwrap());
    assert_eq!(app.active_ws, target_ws);
    assert_eq!(app.workspaces[target_ws].active_tab, target_tab);
    assert_eq!(app.layout().focus, focused_pane);
    assert_eq!(
        app.pane_location(focused_pane),
        Some((target_ws, target_tab))
    );
    assert!(!app.zoomed, "a focused split exits zoom");
    assert_eq!(app.scroll_pane, None, "the new pane starts at live output");
}

/// A failed background split is removed from its inactive owning layout.
#[test]
fn pane_split_failed_spawn_cleans_inactive_workspace_without_changing_caller() {
    let _env = crate::persist::test_env("pane-split-failed-inactive-workspace");
    let (tx, rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let caller_ws = app.active_ws;
    let caller_tab = app.workspaces[caller_ws].active_tab;
    let caller_pane = app.layout().focus;

    let target_root = crate::persist::config_dir().join("failed-target-workspace");
    std::fs::create_dir_all(&target_root).unwrap();
    assert!(app.create_workspace_at(target_root));
    let target_ws = app.active_ws;
    let target_tab = app.workspaces[target_ws].active_tab;
    let target_pane = app.layout().focus;

    app.active_ws = caller_ws;
    app.workspaces[caller_ws].active_tab = caller_tab;
    app.workspaces[caller_ws].tabs[caller_tab].layout.focus = caller_pane;
    app.zoomed = true;
    app.config.shell = "luvus-not-a-real-shell-deferred-split".to_string();

    let out = app
        .dispatch(
            "pane.split",
            &json!({"pane": target_pane.0.to_string(), "focus": false}),
        )
        .unwrap();
    let dead_pane = PaneId(out["pane"].as_str().unwrap().parse().unwrap());
    assert_eq!(app.pane_location(dead_pane), Some((target_ws, target_tab)));

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "the deferred spawn did not fail");
        match rx.recv_timeout(remaining) {
            Ok(AppEvent::PtyExit(id)) if id == dead_pane => {
                app.handle_event(AppEvent::PtyExit(id));
                break;
            }
            Ok(AppEvent::PtyReady { id, .. }) if id == dead_pane => {
                panic!("the deliberately invalid shell unexpectedly spawned")
            }
            Ok(_) => {}
            Err(error) => panic!("the deferred spawn did not fail: {error}"),
        }
    }

    assert!(
        !app.workspaces[target_ws].tabs[target_tab]
            .layout
            .contains(dead_pane),
        "the owning inactive layout drops the dead leaf"
    );
    assert_eq!(
        app.workspaces[target_ws].tabs[target_tab].layout.leaves(),
        vec![target_pane]
    );
    assert_eq!(
        app.workspaces[target_ws].tabs[target_tab].layout.focus,
        target_pane
    );
    assert_eq!(app.pane_location(dead_pane), None);
    assert_eq!(app.active_ws, caller_ws);
    assert_eq!(app.workspaces[caller_ws].active_tab, caller_tab);
    assert_eq!(app.layout().focus, caller_pane);
    assert!(app.zoomed, "the caller's zoom state is preserved");
    assert!(!app.panes.contains_key(&dead_pane));
    assert!(!app.status.contains_key(&dead_pane));
}

/// Closing an inactive workspace through pane teardown publishes one removal.
#[test]
fn closing_last_inactive_workspace_pane_emits_one_workspace_closed_event() {
    let _env = crate::persist::test_env("close-last-inactive-workspace-pane");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let caller_ws = app.active_ws;
    let caller_tab = app.workspaces[caller_ws].active_tab;
    let caller_pane = app.layout().focus;

    let target_root = crate::persist::config_dir().join("close-event-target-workspace");
    std::fs::create_dir_all(&target_root).unwrap();
    assert!(app.create_workspace_at(target_root));
    let target_ws = app.active_ws;
    let target_workspace_id = app.workspaces[target_ws].id.clone();
    let target_pane = app.layout().focus;

    app.active_ws = caller_ws;
    app.workspaces[caller_ws].active_tab = caller_tab;
    app.workspaces[caller_ws].tabs[caller_tab].layout.focus = caller_pane;
    app.zoomed = true;
    let event_floor = crate::ipc::api::current_sequence(&app.events);

    app.handle_event(AppEvent::PtyExit(target_pane));

    assert_eq!(app.workspaces.len(), 1);
    assert!(
        app.workspaces
            .iter()
            .all(|workspace| workspace.id != target_workspace_id),
        "the inactive workspace is removed"
    );
    assert_eq!(app.active_ws, caller_ws);
    assert_eq!(app.workspaces[caller_ws].active_tab, caller_tab);
    assert_eq!(app.layout().focus, caller_pane);
    assert!(app.zoomed, "the caller's zoom state is preserved");
    assert!(!app.panes.contains_key(&target_pane));
    assert!(!app.status.contains_key(&target_pane));

    let events = crate::ipc::api::replayed_events_after(&app.events, event_floor);
    let workspace_closed: Vec<_> = events
        .iter()
        .filter(|event| event["event"] == "workspace.closed")
        .collect();
    assert_eq!(workspace_closed.len(), 1);
    assert_eq!(
        workspace_closed[0]["data"],
        json!({"workspace": target_ws.to_string()})
    );
}

#[test]
fn workspace_organization_api_renames_pins_lists_and_validates() {
    let _env = crate::persist::test_env("workspace-organization-api");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let root = std::env::temp_dir().join(format!(
        "luvus-workspace-organization-{}",
        std::process::id()
    ));
    let a = root.join("a");
    let b = root.join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    assert!(app.create_workspace_at(a.clone()));
    assert!(app.create_workspace_at(b.clone()));
    // The test may run with TMPDIR inside the Luvus checkout. Keep this
    // fixture independent from its parent repository so pin ordering is
    // tested without the separate worktree-grouping behavior.
    for workspace in &mut app.workspaces {
        workspace.worktree = None;
    }
    app.workspaces[0].name = "zero".into();
    app.workspaces[1].name = "one".into();
    app.workspaces[2].name = "two".into();
    app.session_dirty = false;
    assert_eq!(app.active_ws, 2);

    let renamed = app
        .dispatch(
            "workspace.rename",
            &json!({"workspace": "1", "name": "  Luvus website  "}),
        )
        .expect("valid workspace rename");
    assert_eq!(renamed["type"], "workspace_rename");
    assert_eq!(renamed["workspace"], "1");
    assert_eq!(renamed["name"], "Luvus website");
    assert_eq!(renamed["cwd"], a.display().to_string());
    assert_eq!(renamed["pinned"], false);
    assert_eq!(renamed["display_position"], "1");
    assert_eq!(app.active_ws, 2, "rename does not change focus");
    assert!(app.session_dirty);

    app.session_dirty = false;
    let pinned = app
        .dispatch("workspace.pin", &json!({"workspace": 2, "pinned": true}))
        .expect("valid workspace pin");
    assert_eq!(pinned["type"], "workspace_pin");
    assert_eq!(pinned["pinned"], true);
    assert_eq!(pinned["display_position"], "0");
    assert_eq!(app.active_ws, 2, "pin does not change focus");
    assert!(app.session_dirty);

    let listed = app
        .dispatch("workspace.list", &json!({}))
        .expect("workspace list");
    let rows = listed["workspaces"].as_array().unwrap();
    assert_eq!(rows[0]["workspace"], "0", "API order stays stable");
    assert_eq!(rows[1]["name"], "Luvus website");
    assert_eq!(rows[1]["cwd"], a.display().to_string());
    assert_eq!(rows[1]["terminal_cwd"], a.display().to_string());
    assert_eq!(rows[1]["pinned"], false);
    assert_eq!(rows[1]["display_position"], "2");
    assert_eq!(rows[2]["workspace"], "2");
    assert_eq!(rows[2]["pinned"], true);
    assert_eq!(rows[2]["display_position"], "0");
    let fetched = app
        .dispatch("workspace.get", &json!({"workspace": 1}))
        .expect("workspace get");
    assert_eq!(fetched["terminal_cwd"], a.display().to_string());

    let unpinned = app
        .dispatch("workspace.pin", &json!({"workspace": "2", "pinned": false}))
        .expect("valid workspace unpin");
    assert_eq!(unpinned["pinned"], false);
    assert_eq!(unpinned["display_position"], "2");

    let before = app.workspaces[1].name.clone();
    for (method, params, code) in [
        (
            "workspace.rename",
            json!({"name": "missing"}),
            "invalid_request",
        ),
        (
            "workspace.rename",
            json!({"workspace": 99, "name": "missing"}),
            "not_found",
        ),
        (
            "workspace.rename",
            json!({"workspace": 1, "name": "   "}),
            "invalid_request",
        ),
        (
            "workspace.rename",
            json!({"workspace": 1, "name": "x".repeat(41)}),
            "invalid_request",
        ),
        (
            "workspace.pin",
            json!({"workspace": 1, "pinned": "yes"}),
            "invalid_request",
        ),
        (
            "workspace.pin",
            json!({"workspace": 99, "pinned": true}),
            "not_found",
        ),
    ] {
        let err = app.dispatch(method, &params).expect_err("invalid mutation");
        assert_eq!(err.0, code, "method={method} params={params}");
    }
    assert_eq!(app.workspaces[1].name, before, "failed rename is atomic");
    assert!(!app.workspaces[1].pinned, "failed pin is atomic");

    drop(app);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn pane_move_api_moves_to_new_and_existing_tabs_without_restarting() {
    let _env = crate::persist::test_env("pane-move-api");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let a = app.layout().focus;
    app.split(crate::layout::Axis::Col);
    let b = app.layout().focus;

    let out = app
        .dispatch(
            "pane.move",
            &json!({"pane": b.0.to_string(), "new_tab": true}),
        )
        .expect("split pane can move to a fresh tab");
    assert_eq!(out["type"], "pane_move");
    assert_eq!(out["pane"], b.0.to_string());
    assert_eq!(out["tab"], "2");
    assert_eq!(app.workspaces[0].tabs.len(), 2);
    assert!(app.panes.contains_key(&a) && app.panes.contains_key(&b));

    // Resolve A globally while B's destination tab is active. A's source tab
    // empties and collapses, so the old tab 2 becomes final tab 1.
    let out = app
        .dispatch("pane.move", &json!({"pane": a.0.to_string(), "tab": 2}))
        .expect("pane id resolves outside the active tab");
    assert_eq!(out["tab"], "1");
    assert_eq!(app.workspaces[0].tabs.len(), 1);
    let leaves = app.layout().leaves();
    assert!(leaves.contains(&a) && leaves.contains(&b));
    assert_eq!(app.layout().focus, a, "focus follows the moved pane");
    assert!(app.panes.contains_key(&a), "the existing PTY remains live");
}

#[test]
fn pane_move_api_validates_destination_shape_and_range() {
    let _env = crate::persist::test_env("pane-move-api-invalid");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    let original = app.layout().leaves();

    for params in [
        json!({"pane": pane.0.to_string()}),
        json!({"pane": pane.0.to_string(), "tab": 1, "new_tab": true}),
        json!({"pane": pane.0.to_string(), "tab": 0}),
        json!({"pane": pane.0.to_string(), "tab": 9}),
        json!({"pane": pane.0.to_string(), "new_tab": "yes"}),
    ] {
        let err = app
            .dispatch("pane.move", &params)
            .expect_err("invalid pane move must fail");
        assert_eq!(err.0, "invalid_request", "params: {params}");
        assert_eq!(app.layout().leaves(), original, "failure is atomic");
    }
}

#[test]
fn tab_move_api_reorders_and_preserves_active_tab() {
    let _env = crate::persist::test_env("tab-move-api");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    app.workspaces[0].tabs[0].name = Some("a".into());
    app.run_cmd(crate::app::keys::Cmd::NewTab);
    app.workspaces[0].tabs[1].name = Some("b".into());
    app.run_cmd(crate::app::keys::Cmd::NewTab);
    app.workspaces[0].tabs[2].name = Some("c".into());

    let out = app
        .dispatch("tab.move", &json!({"tab": "1", "to": 3}))
        .expect("valid tab reorder");
    assert_eq!(
        out,
        json!({
            "type": "tab_move",
            "from": "1",
            "to": "3",
            "active": "2",
        })
    );
    let names = app
        .ws()
        .tabs
        .iter()
        .map(|tab| tab.name.as_deref().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names, ["b", "c", "a"]);
    assert_eq!(
        app.ws().tabs[app.ws().active_tab].name.as_deref(),
        Some("c")
    );

    let out = app
        .dispatch("tab.move", &json!({"tab": 3, "to": 1, "direction": null}))
        .expect("null direction uses explicit tab positions");
    assert_eq!(
        out,
        json!({
            "type": "tab_move",
            "from": "3",
            "to": "1",
            "active": "3",
        })
    );
    let names = app
        .ws()
        .tabs
        .iter()
        .map(|tab| tab.name.as_deref().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names, ["a", "b", "c"]);
    assert_eq!(
        app.ws().tabs[app.ws().active_tab].name.as_deref(),
        Some("c")
    );

    for params in [
        json!({"tab": 0, "to": 1}),
        json!({"tab": 1, "to": 1}),
        json!({"tab": 1, "to": 9}),
        json!({"tab": 1}),
    ] {
        let err = app
            .dispatch("tab.move", &params)
            .expect_err("invalid tab move must fail");
        assert_eq!(err.0, "invalid_request", "params: {params}");
    }
}

#[test]
fn tab_move_api_supports_directional_active_and_explicit_targets() {
    let _env = crate::persist::test_env("tab-move-direction-api");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    app.workspaces[0].tabs[0].name = Some("a".into());
    app.run_cmd(crate::app::keys::Cmd::NewTab);
    app.workspaces[0].tabs[1].name = Some("b".into());
    app.run_cmd(crate::app::keys::Cmd::NewTab);
    app.workspaces[0].tabs[2].name = Some("c".into());

    let out = app
        .dispatch("tab.move", &json!({"direction": "left"}))
        .expect("active tab moves left");
    assert_eq!(
        out,
        json!({"type":"tab_move", "from":"3", "to":"2", "active":"2"})
    );
    let names = |app: &App| {
        app.ws()
            .tabs
            .iter()
            .map(|tab| tab.name.clone().unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(&app), ["a", "c", "b"]);

    let out = app
        .dispatch("tab.move", &json!({"direction": "right", "tab": 1}))
        .expect("explicit tab moves right");
    assert_eq!(
        out,
        json!({"type":"tab_move", "from":"1", "to":"2", "active":"1"})
    );
    assert_eq!(names(&app), ["c", "a", "b"]);
    assert_eq!(
        app.ws().tabs[app.ws().active_tab].name.as_deref(),
        Some("c"),
        "active tab identity is preserved"
    );

    for params in [
        json!({"direction": "left", "tab": 1}),
        json!({"direction": "right", "tab": 3}),
        json!({"direction": "up"}),
        json!({"direction": "left", "to": 1}),
        json!({"direction": "right", "tab": 0}),
    ] {
        let before = names(&app);
        let err = app
            .dispatch("tab.move", &params)
            .expect_err("invalid directional move must fail");
        assert_eq!(err.0, "invalid_request", "params: {params}");
        assert_eq!(names(&app), before, "failure is atomic: {params}");
    }
}

#[test]
fn tab_swap_api_exchanges_positions_and_preserves_active_identity() {
    let _env = crate::persist::test_env("tab-swap-api");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    app.workspaces[0].tabs[0].name = Some("a".into());
    app.run_cmd(crate::app::keys::Cmd::NewTab);
    app.workspaces[0].tabs[1].name = Some("b".into());
    app.run_cmd(crate::app::keys::Cmd::NewTab);
    app.workspaces[0].tabs[2].name = Some("c".into());

    let out = app
        .dispatch("tab.swap", &json!({"tab": 1, "with": "3"}))
        .expect("valid tab swap");
    assert_eq!(
        out,
        json!({"type":"tab_swap", "tab":"1", "with":"3", "active":"1"})
    );
    let names = |app: &App| {
        app.ws()
            .tabs
            .iter()
            .map(|tab| tab.name.clone().unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(&app), ["c", "b", "a"]);
    assert_eq!(
        app.ws().tabs[app.ws().active_tab].name.as_deref(),
        Some("c")
    );

    for params in [
        json!({}),
        json!({"tab": 0, "with": 1}),
        json!({"tab": 1, "with": 1}),
        json!({"tab": 1, "with": 9}),
    ] {
        let before = names(&app);
        let err = app
            .dispatch("tab.swap", &params)
            .expect_err("invalid tab swap must fail");
        assert_eq!(err.0, "invalid_request", "params: {params}");
        let after = names(&app);
        assert_eq!(after, before, "failure is atomic: {params}");
    }
}

#[test]
fn tab_focus_api_requires_an_existing_one_based_position() {
    let _env = crate::persist::test_env("tab-focus-api");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    app.run_cmd(crate::app::keys::Cmd::NewTab);

    assert_eq!(
        app.dispatch("tab.focus", &json!({"tab": "1"})),
        Ok(json!({"type": "ok"}))
    );
    assert_eq!(app.ws().active_tab, 0);

    for params in [json!({}), json!({"tab": 0}), json!({"tab": 3})] {
        let before = app.ws().active_tab;
        let err = app
            .dispatch("tab.focus", &params)
            .expect_err("invalid focus must fail");
        assert_eq!(err.0, "invalid_request", "params: {params}");
        assert_eq!(app.ws().active_tab, before, "failure is atomic");
    }
}

#[test]
fn tab_rename_api_validates_target_name_and_dashboard_kind() {
    let _env = crate::persist::test_env("tab-rename-api");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    app.run_cmd(crate::app::keys::Cmd::NewTab);

    app.dispatch("tab.rename", &json!({"name": "active"}))
        .expect("omitting tab targets the active tab");
    assert_eq!(app.ws().tabs[1].name.as_deref(), Some("active"));

    app.dispatch("tab.rename", &json!({"tab": 1, "name": " first "}))
        .expect("explicit one-based tab is accepted");
    assert_eq!(app.ws().tabs[0].name.as_deref(), Some("first"));
    app.dispatch("tab.rename", &json!({"tab": 1, "name": ""}))
        .expect("an explicit empty name clears the label");
    assert_eq!(app.ws().tabs[0].name, None);

    let names = |app: &App| {
        app.ws()
            .tabs
            .iter()
            .map(|tab| tab.name.clone())
            .collect::<Vec<_>>()
    };
    for params in [
        json!({"tab": 0, "name": "wrong"}),
        json!({"tab": "nope", "name": "wrong"}),
        json!({"tab": 9, "name": "wrong"}),
        json!({"tab": 1}),
        json!({"tab": 1, "name": 7}),
        json!({"tab": 1, "name": "x".repeat(41)}),
    ] {
        let before = names(&app);
        let err = app
            .dispatch("tab.rename", &params)
            .expect_err("invalid rename must fail");
        assert_eq!(err.0, "invalid_request", "params: {params}");
        assert_eq!(names(&app), before, "failure is atomic: {params}");
    }

    app.open_mission_control(0);
    let mission = app.ws().active_tab + 1;
    let err = app
        .dispatch("tab.rename", &json!({"tab": mission, "name": "wrong"}))
        .expect_err("dashboard rename must fail");
    assert_eq!(err.0, "invalid_request");
    assert!(app.ws().tabs[mission - 1].name.is_none());
}

#[test]
fn agent_get_returns_one_agents_info() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    app.status.get_mut(&pane).unwrap().agent = "claude".into();
    app.set_agent_name(pane, Some("worker"));

    let out = app
        .dispatch("agent.get", &json!({"target": "worker"}))
        .expect("agent.get ok");
    assert_eq!(out["pane"], pane.0.to_string());
    assert_eq!(out["name"], "worker");
    assert_eq!(out["agent"], "claude");
    // Resolves by kind too.
    let by_kind = app
        .dispatch("agent.get", &json!({"target": "claude"}))
        .unwrap();
    assert_eq!(by_kind["pane"], pane.0.to_string());
}

#[test]
fn agent_read_accepts_a_source() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus.0.to_string();
    for src in ["visible", "recent"] {
        let out = app
            .dispatch("agent.read", &json!({"target": pane, "source": src}))
            .expect("agent.read ok");
        assert!(out["text"].is_string(), "{src} returns text");
    }
}

#[test]
fn pane_inspection_reports_read_only_history_metrics() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    if let Some(p) = app.panes.get(&pane) {
        if let Ok(mut engine) = p.engine.lock() {
            for i in 0..300 {
                engine.advance(format!("line {i}\r\n").as_bytes());
            }
            engine.finish_output_batch();
        }
    }
    let out = app
        .dispatch("pane.status", &json!({"pane": pane.0.to_string()}))
        .expect("pane status");
    assert_eq!(out["type"], "pane_status");
    assert!(out["history_budget_bytes"].as_u64().unwrap_or(0) > 0);
    assert!(out["history_rows"].as_u64().is_some());
    assert!(out["history_bytes"].as_u64().is_some());
    assert!(out["history_estimated_grid_bytes"].as_u64().is_some());
    assert!(out.get("history_cache_bytes").is_some());
    assert!(out["history_compacted_rows"].as_u64().is_some());
    assert!(out["history_allocated_cells"].as_u64().is_some());
    assert!(out["history_packed_blocks"].as_u64().is_some());
    assert!(out["history_packed_bytes"].as_u64().is_some());
    assert!(out["history_packed_rows"].as_u64().is_some());
    assert!(out["history_packed_rows"].as_u64().unwrap_or(0) > 0);
    assert!(out["history_dense_row_bytes"].as_u64().is_some());
    assert!(out["history_row_descriptor_bytes"].as_u64().is_some());
    assert!(out["history_allocation_count"].as_u64().is_some());
    assert_eq!(out["history_bytes_kind"], "estimated");
    assert_eq!(out["history_exact"], false, "Alacritty reports an estimate");

    let listed = app.dispatch("pane.list", &json!({})).expect("pane list");
    assert!(listed["detection_extractions"].as_u64().is_some());
    assert!(listed["detection_skips"].as_u64().is_some());
    assert!(listed["detection_performance"]["panes_considered"]
        .as_u64()
        .is_some());
    assert!(listed["detection_performance"]["full_fleet_audits"]
        .as_u64()
        .is_some());
    assert!(listed["detection_performance"]["audit_recoveries"]
        .as_u64()
        .is_some());
    assert!(listed["render_performance"]["frames_sent"]
        .as_u64()
        .is_some());
    assert!(listed["render_performance"]["render_passes"]
        .as_u64()
        .is_some());
    let row = listed["panes"].as_array().unwrap().first().unwrap();
    assert!(row.get("scroll_offset").is_some());
    assert!(row.get("history_budget_bytes").is_some());
    assert!(row.get("history_estimated_grid_bytes").is_some());
    assert!(row.get("history_cache_bytes").is_some());
    assert!(row.get("history_compacted_rows").is_some());
    assert!(row.get("history_allocated_cells").is_some());
    assert!(row.get("history_packed_blocks").is_some());
    assert!(row.get("history_packed_bytes").is_some());
    assert!(row.get("history_packed_rows").is_some());
    assert!(row.get("history_dense_row_bytes").is_some());
    assert!(row.get("history_row_descriptor_bytes").is_some());
    assert!(row.get("history_allocation_count").is_some());
    assert_eq!(row["history_bytes_kind"], "estimated");
}

#[test]
fn rename_pane_is_offered_in_both_menus() {
    use crate::app::{AgentMenu, AgentMenuItem, AgentTarget, PaneMenuItem};
    let (tx, _rx) = std::sync::mpsc::channel();
    let app = App::new(80, 24, tx).unwrap();
    assert!(app.pane_menu_items().contains(&PaneMenuItem::RenamePane));
    let pane = app.layout().focus;
    assert!(AgentMenu::items_for(AgentTarget::Live(pane)).contains(&AgentMenuItem::RenamePane));
}

#[test]
fn agent_name_grammar_is_cli_safe() {
    assert!(valid_agent_name("reviewer"));
    assert!(valid_agent_name("a1_x-y"));
    assert!(!valid_agent_name("")); // empty
    assert!(!valid_agent_name("1abc")); // must start with a letter
    assert!(!valid_agent_name("Bad")); // uppercase
    assert!(!valid_agent_name("has space"));
    assert!(!valid_agent_name(&"x".repeat(33))); // too long
}

#[test]
fn runtime_string_limits_count_unicode_codepoints() {
    let accepted = "é".repeat(MAX_AGENT_REPORT_MESSAGE_CHARS);
    assert_eq!(
        optional_bounded_string(&json!({"message":accepted}), "message", 4096)
            .unwrap()
            .unwrap()
            .chars()
            .count(),
        4096
    );
    let rejected = "é".repeat(MAX_AGENT_REPORT_MESSAGE_CHARS + 1);
    assert!(optional_bounded_string(&json!({"message":rejected}), "message", 4096).is_err());
}

/// `wait.output` never polls: an already-visible marker resolves on
/// registration, fresh output resolves on the next output event, and a
/// deadline lapses on the loop tick (docs/81).
#[test]
fn wait_output_resolves_immediately_or_on_output_or_deadline() {
    use std::sync::mpsc::{Receiver, RecvTimeoutError};
    let _env = crate::persist::test_env("wait-output");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;

    // Immediate: the marker is already in the pane's recent output.
    app.panes
        .get(&pane)
        .unwrap()
        .engine
        .lock()
        .unwrap()
        .advance(b"ready NOW\r\n");
    let (reply, rx): (_, Receiver<String>) = std::sync::mpsc::channel();
    app.register_output_wait(
        pane,
        "t1".into(),
        "NOW".into(),
        reply,
        None,
        Arc::new(AtomicBool::new(false)),
    );
    assert!(rx
        .recv_timeout(Duration::from_secs(1))
        .unwrap()
        .contains("\"matched\":true"));

    // Parked: resolves only when the pane produces matching output.
    let (reply, rx): (_, Receiver<String>) = std::sync::mpsc::channel();
    app.register_output_wait(
        pane,
        "t2".into(),
        "LATER".into(),
        reply,
        None,
        Arc::new(AtomicBool::new(false)),
    );
    assert_eq!(
        rx.recv_timeout(Duration::from_millis(50)),
        Err(RecvTimeoutError::Timeout)
    );
    app.panes
        .get(&pane)
        .unwrap()
        .engine
        .lock()
        .unwrap()
        .advance(b"arrives LATER\r\n");
    app.check_output_waits(pane);
    assert!(rx
        .recv_timeout(Duration::from_secs(1))
        .unwrap()
        .contains("\"matched\":true"));

    // Deadline: an unmatched waiter lapses on the tick.
    let (reply, rx): (_, Receiver<String>) = std::sync::mpsc::channel();
    app.register_output_wait(
        pane,
        "t3".into(),
        "NEVER".into(),
        reply,
        Some(Duration::from_millis(10)),
        Arc::new(AtomicBool::new(false)),
    );
    app.tick_output_waits(Instant::now() + Duration::from_secs(1));
    assert!(rx
        .recv_timeout(Duration::from_secs(1))
        .unwrap()
        .contains("\"matched\":false"));

    // A closed pane fails its parked waiters.
    let (reply, rx): (_, Receiver<String>) = std::sync::mpsc::channel();
    app.register_output_wait(
        pane,
        "t4".into(),
        "NEVER".into(),
        reply,
        None,
        Arc::new(AtomicBool::new(false)),
    );
    app.cancel_output_waits(pane);
    assert!(rx
        .recv_timeout(Duration::from_secs(1))
        .unwrap()
        .contains("\"matched\":false"));
    assert!(app.output_waits.is_empty(), "no waiters leak");
}

/// A waiter registered without `timeout_s` still gets a bounded deadline, so
/// a disconnected client cannot leave it parked for the life of the pane.
#[test]
fn output_wait_without_timeout_is_bounded() {
    let _env = crate::persist::test_env("wait-bound");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    let (reply, _rx): (_, std::sync::mpsc::Receiver<String>) = std::sync::mpsc::channel();
    app.register_output_wait(
        pane,
        "t".into(),
        "NEVER".into(),
        reply,
        None,
        Arc::new(AtomicBool::new(false)),
    );
    let waiter = &app.output_waits[&pane][0];
    assert!(waiter.deadline.is_some(), "an abandoned waiter must expire");
}

#[test]
fn disconnected_clients_reclaim_parked_waiters_without_replies() {
    use std::sync::mpsc::TryRecvError;

    let _env = crate::persist::test_env("wait-disconnect");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;

    let output_cancelled = Arc::new(AtomicBool::new(false));
    let (output_reply, output_rx) = std::sync::mpsc::channel();
    app.register_output_wait(
        pane,
        "output-disconnect".into(),
        "NEVER".into(),
        output_reply,
        None,
        output_cancelled.clone(),
    );

    let agent_cancelled = Arc::new(AtomicBool::new(false));
    let (agent_reply, agent_rx) = std::sync::mpsc::channel();
    app.register_agent_wait(
        pane,
        "agent-disconnect".into(),
        vec![State::Blocked],
        agent_reply,
        None,
        agent_cancelled.clone(),
    );

    output_cancelled.store(true, Ordering::Release);
    agent_cancelled.store(true, Ordering::Release);
    let now = Instant::now();
    app.tick_output_waits(now);
    app.tick_agent_waits(now);

    assert!(app.output_waits.is_empty());
    assert!(app.agent_waits.is_empty());
    assert_eq!(output_rx.try_recv(), Err(TryRecvError::Disconnected));
    assert_eq!(agent_rx.try_recv(), Err(TryRecvError::Disconnected));
}

#[test]
fn agent_wait_matches_each_state_and_reports_the_actual_transition() {
    let _env = crate::persist::test_env("agent-wait-state-set");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;

    for target in [State::Idle, State::Working, State::Blocked, State::Done] {
        app.status.get_mut(&pane).unwrap().state = if target == State::Idle {
            State::Working
        } else {
            State::Idle
        };
        let (reply, response) = std::sync::mpsc::channel();
        app.register_agent_wait(
            pane,
            format!("wait-{target:?}"),
            vec![target],
            reply,
            Some(Duration::from_secs(1)),
            Arc::new(AtomicBool::new(false)),
        );
        app.status.get_mut(&pane).unwrap().state = target;
        app.check_agent_waits(pane);
        let value: Value = serde_json::from_str(&response.recv().unwrap()).unwrap();
        assert_eq!(value["result"]["matched"], true);
        assert_eq!(value["result"]["status"], state_str(target));
    }

    app.status.get_mut(&pane).unwrap().state = State::Idle;
    let (reply, response) = std::sync::mpsc::channel();
    app.register_agent_wait(
        pane,
        "wait-terminal".into(),
        vec![State::Working, State::Done],
        reply,
        Some(Duration::from_secs(1)),
        Arc::new(AtomicBool::new(false)),
    );
    app.status.get_mut(&pane).unwrap().state = State::Done;
    app.check_agent_waits(pane);
    let value: Value = serde_json::from_str(&response.recv().unwrap()).unwrap();
    assert_eq!(value["result"]["status"], "done");
}

#[test]
fn agent_wait_status_set_matches_current_state_and_times_out_bounded() {
    let _env = crate::persist::test_env("agent-wait-current-timeout");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    app.status.get_mut(&pane).unwrap().state = State::Done;

    let (reply, response) = std::sync::mpsc::channel();
    app.register_agent_wait(
        pane,
        "already-done".into(),
        vec![State::Working, State::Done],
        reply,
        Some(Duration::from_secs(1)),
        Arc::new(AtomicBool::new(false)),
    );
    let value: Value = serde_json::from_str(&response.recv().unwrap()).unwrap();
    assert_eq!(value["result"]["matched"], true);
    assert_eq!(value["result"]["status"], "done");

    let (reply, response) = std::sync::mpsc::channel();
    app.register_agent_wait(
        pane,
        "timeout".into(),
        vec![State::Working, State::Blocked],
        reply,
        Some(Duration::ZERO),
        Arc::new(AtomicBool::new(false)),
    );
    app.tick_agent_waits(Instant::now());
    let value: Value = serde_json::from_str(&response.recv().unwrap()).unwrap();
    assert_eq!(value["result"]["matched"], false);
    assert_eq!(value["result"]["status"], "done");
}

/// Every pane-close path funnels through `drop_leaf_runtime`, so closing a
/// workspace or tab must fail its parked waiters rather than leaking them.
#[test]
fn closing_a_workspace_cancels_parked_waiters() {
    let _env = crate::persist::test_env("wait-close-ws");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    let (reply, rx): (_, std::sync::mpsc::Receiver<String>) = std::sync::mpsc::channel();
    app.register_output_wait(
        pane,
        "t".into(),
        "NEVER".into(),
        reply,
        None,
        Arc::new(AtomicBool::new(false)),
    );
    app.close_workspace(0);
    assert!(
        rx.recv_timeout(Duration::from_secs(1))
            .unwrap()
            .contains("\"matched\":false"),
        "a closed workspace fails its parked waiters"
    );
    assert!(app.output_waits.is_empty(), "no waiters leak");
}
