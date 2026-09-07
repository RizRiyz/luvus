use super::super::*;
use super::support::*;
#[test]
fn config_patch_rejects_unknown_fields_without_mutation() {
    let (_env, mut app) = app("socket-config");
    let before = serde_json::to_value(&app.config).unwrap();
    let error = app.dispatch(
        "config.patch",
        &json!({"patch":{"layout":{"unknown":true}}}),
    );
    assert!(error.is_err());
    assert_eq!(serde_json::to_value(&app.config).unwrap(), before);

    let result = app
        .dispatch("config.patch", &json!({"patch":{"check_updates":false}}))
        .unwrap();
    assert_eq!(result["config"]["check_updates"], false);
    assert!(!app.config.check_updates);

    app.dispatch(
        "config.patch",
        &json!({"patch":{"keybindings":{"new-command":"ctrl+n"}}}),
    )
    .unwrap();
    assert_eq!(
        app.config
            .keybindings
            .get("new-command")
            .map(String::as_str),
        Some("ctrl+n")
    );
    app.dispatch(
        "config.patch",
        &json!({"patch":{"direct_keybindings":{"next_tab":"alt+right"}}}),
    )
    .unwrap();
    assert_eq!(
        keys::direct_command(
            &app.direct_keymap,
            &ratatui::crossterm::event::KeyEvent::new(
                ratatui::crossterm::event::KeyCode::Right,
                ratatui::crossterm::event::KeyModifiers::ALT,
            ),
        ),
        Some(keys::Cmd::NextTab)
    );
    let direct_before = app.config.direct_keybindings.clone();
    assert!(app
        .dispatch(
            "config.patch",
            &json!({"patch":{"direct_keybindings":{"next_tab":"\u{1b}[1;3C"}}}),
        )
        .is_err());
    assert_eq!(app.config.direct_keybindings, direct_before);
    app.dispatch(
        "config.patch",
        &json!({"patch":{"mission_pricing":{"new-model":[1.0,2.0,0.5]}}}),
    )
    .unwrap();
    assert_eq!(
        app.config.mission_pricing.get("new-model"),
        Some(&[1.0, 2.0, 0.5])
    );

    app.agents_scroll = 9;
    let result = app
        .dispatch(
            "config.patch",
            &json!({"patch":{"agents_active_only":true,"agents_this_workspace":true}}),
        )
        .unwrap();
    assert_eq!(result["config"]["agents_active_only"], true);
    assert_eq!(result["config"]["agents_this_workspace"], true);
    assert!(app.agents_active_only);
    assert!(app.agents_this_workspace);
    assert_eq!(app.agents_scroll, 0);
}

#[test]
fn config_reload_applies_the_agents_filter_live() {
    let (_env, mut app) = app("socket-config-agents-reload");
    app.agents_active_only = true;
    app.config.agents_active_only = true;
    app.agents_this_workspace = true;
    app.config.agents_this_workspace = true;
    app.agents_scroll = 8;

    crate::config::save(&crate::config::Config::default());
    let result = app.dispatch("server.reload_config", &json!({})).unwrap();

    assert_eq!(result["config"]["agents_active_only"], false);
    assert_eq!(result["config"]["agents_this_workspace"], false);
    assert!(!app.agents_active_only);
    assert!(!app.agents_this_workspace);
    assert_eq!(app.agents_scroll, 0);
}

/// Automation is told to discover graphics support rather than infer it
/// from a release number, so the two halves of the answer have to be there
/// and have to mean different things: what this build implements, and
/// whether an image would reach a screen right now.
#[test]
fn capabilities_separate_graphics_support_from_present_availability() {
    let (_env, mut app) = app("socket-graphics-capabilities");

    let reported = app
        .dispatch("uhp.capabilities", &serde_json::json!({}))
        .expect("capabilities are reported");
    let graphics = &reported["graphics"];
    assert_eq!(graphics["protocol"], "kitty");
    assert_eq!(graphics["placement"], "unicode_placeholder");
    assert_eq!(graphics["supported"], true, "the build implements it");
    assert_eq!(
        graphics["available"], false,
        "no client is attached, so nothing could be drawn"
    );

    app.set_host_graphics(true);
    let reported = app
        .dispatch("uhp.capabilities", &serde_json::json!({}))
        .expect("capabilities are reported");
    assert_eq!(
        reported["graphics"]["available"], true,
        "a client that can draw has attached"
    );
}

#[test]
fn config_patch_updates_child_appearance_and_notifies_mode_2031() {
    let (_env, mut app) = app("socket-theme-appearance");
    let pane_id = app.layout().focus;
    let (response_tx, response_rx) = std::sync::mpsc::channel();
    let mut engine = crate::terminal::vt::alacritty::AlacrittyEngine::with_appearance(
        80,
        24,
        response_tx,
        crate::config::SCROLLBACK_BYTES_DEFAULT,
        crate::terminal::appearance::PaneAppearance::default(),
        crate::terminal::graphics::HostGraphics::default(),
    );
    crate::terminal::vt::VtEngine::advance(&mut engine, b"\x1b[?2031h");
    app.panes.get_mut(&pane_id).unwrap().engine =
        std::sync::Arc::new(std::sync::Mutex::new(engine));

    app.dispatch("config.patch", &json!({"patch":{"theme":"gruvbox-light"}}))
        .unwrap();
    let recv_bytes = || match response_rx.recv().unwrap() {
        crate::terminal::pty::InputAction::Bytes(bytes) => bytes,
        crate::terminal::pty::InputAction::Submit { .. } => panic!("unexpected submit"),
    };
    assert_eq!(recv_bytes(), b"\x1b[?997;2n");

    app.panes[&pane_id]
        .engine
        .lock()
        .unwrap()
        .advance(b"\x1b]11;?\x07");
    assert_eq!(recv_bytes(), b"\x1b]11;rgb:f2f2/e5e5/bcbc\x07");
}

#[test]
fn socket_mutations_support_optimistic_revision_guards() {
    let (_env, mut app) = app("socket-revision-guard");
    let (reply, _) = std::sync::mpsc::channel();
    let first: Value = serde_json::from_str(&app.handle_api(&ApiRequest {
        id: "first".into(),
        method: "tab.new".into(),
        params: json!({"if_revision":0}),
        reply: reply.clone(),
    }))
    .unwrap();
    assert!(first["result"]["revision"].as_u64().unwrap() > 0);

    let conflict: Value = serde_json::from_str(&app.handle_api(&ApiRequest {
        id: "stale".into(),
        method: "tab.new".into(),
        params: json!({"if_revision":0}),
        reply,
    }))
    .unwrap();
    assert_eq!(conflict["error"]["code"], "revision_conflict");
    assert_eq!(app.workspaces[0].tabs.len(), 2);
}
