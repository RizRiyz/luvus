use super::super::*;
use crate::app::App;

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
