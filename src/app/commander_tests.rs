//! Focused Commander integration tests in the App privacy boundary.

use super::*;
use crate::commander::Commander;

#[test]
fn unicode_editing_and_word_delete_keep_valid_boundaries() {
    let mut commander = Commander::default();
    commander.insert("=p17 hello 世界");
    commander.backspace(true);
    assert_eq!(commander.draft, "=p17 hello ");
    commander.cursor = 0;
    commander.delete(true);
    assert_eq!(commander.draft, "p17 hello ");
    commander.insert("λ");
    assert_eq!(commander.draft, "λp17 hello ");
}

#[test]
fn parser_requires_explicit_targets_and_prompt() {
    let _env = crate::persist::test_env("commander-parse");
    let (tx, _) = std::sync::mpsc::channel();
    let app = App::new(80, 24, tx).unwrap();
    assert!(app.commander_parse("hello").is_err());
    assert!(app.commander_parse("=p1 ").is_err());
    assert!(app.commander_parse("=p1 =p1 hello").is_err());
}

#[test]
fn literal_arguments_and_escaped_mentions_remain_in_the_prompt() {
    let _env = crate::persist::test_env("commander-literal-arguments");
    let (tx, _) = std::sync::mpsc::channel();
    let app = App::new(80, 24, tx).unwrap();
    let id = app.layout().focus;
    let draft = format!("=p{} npm install @types/node =value \\@p88 \\=p99", id.0);
    let plan = app.commander_parse(&draft).unwrap();
    assert_eq!(plan.targets.len(), 1);
    assert_eq!(plan.targets[0].pane, id);
    assert_eq!(plan.prompt, "npm install @types/node =value @p88 =p99");
    assert!(app
        .commander_parse(&format!("=p{} echo hi @p999999", id.0))
        .is_err());
}

#[test]
fn inline_mentions_select_multiple_exact_panes_without_entering_the_message() {
    let _env = crate::persist::test_env("commander-mentions");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let first = app.layout().focus;
    app.new_tab();
    let second = app.layout().focus;
    app.status.get_mut(&first).unwrap().agent = "zsh".into();
    app.status.get_mut(&second).unwrap().agent = "zsh".into();
    let (first_tx, first_rx) = std::sync::mpsc::channel();
    let (second_tx, second_rx) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&first)
        .unwrap()
        .replace_input_sender_for_test(first_tx);
    app.panes
        .get_mut(&second)
        .unwrap()
        .replace_input_sender_for_test(second_tx);
    let draft = format!("=p{} echo hello @p{} world", first.0, second.0);
    let parsed = app.commander_parse(&draft).unwrap();
    assert_eq!(parsed.targets.len(), 2);
    assert_eq!(parsed.targets[0].pane, first);
    assert_eq!(parsed.targets[1].pane, second);
    assert_eq!(parsed.prompt, "echo hello world");

    app.open_commander();
    let commander = app.commander.as_mut().unwrap();
    commander.draft = format!("=p{} echo hello", first.0);
    commander.cursor = commander.draft.len();
    app.refresh_commander_preview();
    app.commander_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(app
        .commander
        .as_ref()
        .unwrap()
        .draft
        .contains(&format!("@p{}", second.0)));
    app.commander_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    for rx in [first_rx, second_rx] {
        let crate::terminal::pty::InputAction::Submit { paste, .. } = rx.try_recv().unwrap() else {
            panic!("each selected pane gets one atomic submit");
        };
        assert!(String::from_utf8_lossy(&paste).contains("echo hello"));
        assert!(rx.try_recv().is_err());
    }
    assert_eq!(
        app.commander.as_ref().unwrap().draft,
        format!("=p{} =p{} ", first.0, second.0)
    );
}

#[test]
fn prefix_enter_toggles_the_composer_without_discarding_it_on_send() {
    let _env = crate::persist::test_env("commander-toggle");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let id = app.layout().focus;
    app.status.get_mut(&id).unwrap().agent = "zsh".into();
    let (input_tx, input_rx) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&id)
        .unwrap()
        .replace_input_sender_for_test(input_tx);
    app.open_commander();
    let commander = app.commander.as_mut().unwrap();
    commander.draft = format!("=p{} ls", id.0);
    commander.cursor = commander.draft.len();
    app.commander_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(input_rx.try_recv().is_ok());
    assert!(app.commander.is_some());
    let draft = app.commander.as_ref().unwrap().draft.clone();
    app.handle_event(AppEvent::Mouse(ratatui::crossterm::event::MouseEvent {
        kind: ratatui::crossterm::event::MouseEventKind::Down(
            ratatui::crossterm::event::MouseButton::Left,
        ),
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    }));
    assert_eq!(app.commander.as_ref().unwrap().draft, draft);

    app.handle_event(AppEvent::Key(app.prefix.key_event()));
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    assert!(app.commander.is_none());
    app.handle_event(AppEvent::Key(app.prefix.key_event()));
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    assert!(app.commander.is_some());
    assert!(app.commander.as_ref().unwrap().focused);
    app.handle_event(AppEvent::Key(app.prefix.key_event()));
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    assert!(app.commander.is_none());
}

#[test]
fn editing_chords_selection_and_clipboard_paste_stay_private() {
    let _env = crate::persist::test_env("commander-editing");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    app.open_commander();
    let commander = app.commander.as_mut().unwrap();
    commander.draft = "hello 世界".into();
    commander.cursor = commander.draft.len();
    app.commander_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT));
    assert_eq!(app.commander.as_ref().unwrap().draft, "hello ");
    app.commander_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert!(app.commander.as_ref().unwrap().draft.is_empty());
    app.commander_paste("first");
    app.commander_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::SUPER));
    app.commander_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::SUPER));
    assert_eq!(
        app.commander.as_ref().unwrap().pending_clipboard.as_deref(),
        Some("first")
    );
    assert!(app.pending_clipboard.is_none());
    app.commander_paste("second");
    assert_eq!(app.commander.as_ref().unwrap().draft, "second");
    app.commander_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::SUPER));
    assert!(app.commander.as_ref().unwrap().draft.is_empty());
    app.commander_paste("first\nsecond/path");
    app.commander_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT));
    assert_eq!(app.commander.as_ref().unwrap().draft, "first\nsecond/");
    app.commander_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert_eq!(app.commander.as_ref().unwrap().draft, "first\n");
    app.commander_key(KeyEvent::new(
        KeyCode::Backspace,
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ));
    assert!(app.commander.as_ref().unwrap().draft.is_empty());
}

#[test]
fn clipboard_image_is_discarded_when_draft_is_abandoned_or_edited_away() {
    let _env = crate::persist::test_env("commander-image-paste");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    app.open_commander();
    let png =
        crate::clipboard_image::encode_rgba_png(1, 1, |_, _| [1, 2, 3, 255]).expect("valid png");
    let staged = crate::clipboard_image::stage_png(&png).expect("staged image");
    assert!(app.handle_event(AppEvent::PasteImage(staged.clone())));
    assert!(app
        .commander
        .as_ref()
        .unwrap()
        .draft
        .contains(&staged.to_string_lossy().to_string()));
    assert!(staged.exists());
    app.open_commander();
    assert!(!staged.exists());

    app.open_commander();
    let staged = crate::clipboard_image::stage_png(&png).expect("second staged image");
    assert!(app.handle_event(AppEvent::PasteImage(staged.clone())));
    app.commander_key(KeyEvent::new(
        KeyCode::Backspace,
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ));
    assert!(!staged.exists());
}

#[test]
fn delivered_image_path_survives_composer_close() {
    let _env = crate::persist::test_env("commander-image-delivery");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let id = app.layout().focus;
    let (input_tx, input_rx) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&id)
        .unwrap()
        .replace_input_sender_for_test(input_tx);
    app.open_commander();
    let png =
        crate::clipboard_image::encode_rgba_png(1, 1, |_, _| [1, 2, 3, 255]).expect("valid png");
    let staged = crate::clipboard_image::stage_png(&png).expect("staged image");
    assert!(app.handle_event(AppEvent::PasteImage(staged.clone())));
    app.commander_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(input_rx.try_recv().is_ok());
    app.open_commander();
    assert!(staged.exists());
    crate::clipboard_image::discard_staged_png(&staged);
}

#[test]
fn full_draft_tab_does_not_move_cursor_after_rejected_insert() {
    let _env = crate::persist::test_env("commander-full-draft-tab");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    app.open_commander();
    let commander = app.commander.as_mut().unwrap();
    commander.draft = "λ".repeat(16_384);
    commander.cursor = 0;
    app.commander_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    let commander = app.commander.as_ref().unwrap();
    assert_eq!(commander.cursor, 0);
    assert_eq!(commander.draft.chars().count(), 16_384);
}

#[test]
fn help_overlay_owns_keys_mouse_and_paste_above_the_composer() {
    use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let _env = crate::persist::test_env("commander-help-priority");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    app.open_commander();
    app.commander_area = Some(Rect::new(0, 20, 80, 4));
    let draft = app.commander.as_ref().unwrap().draft.clone();
    app.help_open = true;
    assert!(app.handle_event(AppEvent::Paste("hidden input".into())));
    assert_eq!(app.commander.as_ref().unwrap().draft, draft);
    assert!(app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('x'),
        KeyModifiers::NONE,
    ))));
    assert!(!app.help_open);
    assert_eq!(app.commander.as_ref().unwrap().draft, draft);

    app.help_open = true;
    app.handle_event(AppEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 3,
        row: 21,
        modifiers: KeyModifiers::NONE,
    }));
    assert!(!app.help_open);
    assert_eq!(app.commander.as_ref().unwrap().draft, draft);
}

#[test]
fn exact_shell_target_queues_one_atomic_command() {
    let _env = crate::persist::test_env("commander-shell-submit");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let id = app.layout().focus;
    app.status.get_mut(&id).unwrap().agent = "zsh".into();
    let (input_tx, input_rx) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&id)
        .unwrap()
        .replace_input_sender_for_test(input_tx);

    app.open_commander();
    let commander = app.commander.as_mut().unwrap();
    commander.draft = format!("=p{} ls", id.0);
    commander.cursor = commander.draft.len();
    app.commander_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    let crate::terminal::pty::InputAction::Submit { paste, .. } = input_rx.try_recv().unwrap()
    else {
        panic!("shell command must be one atomic paste-and-Enter action");
    };
    assert!(String::from_utf8_lossy(&paste).contains("ls"));
    assert!(input_rx.try_recv().is_err());
    assert_eq!(
        app.commander.as_ref().unwrap().delivery_results,
        vec![format!("p{}: queued", id.0)]
    );
}

#[test]
fn shell_target_rejects_multiline_and_agent_identity_drift() {
    let _env = crate::persist::test_env("commander-shell-safety");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let id = app.layout().focus;
    app.status.get_mut(&id).unwrap().agent = "zsh".into();
    assert!(app
        .commander_parse(&format!("=p{} echo one\necho two", id.0))
        .is_err());

    let plan = app.commander_parse(&format!("=p{} ls", id.0)).unwrap();
    let (input_tx, input_rx) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&id)
        .unwrap()
        .replace_input_sender_for_test(input_tx);
    app.open_commander();
    app.status.get_mut(&id).unwrap().agent = "codex".into();
    app.commander_dispatch(plan);
    assert!(input_rx.try_recv().is_err());
    assert_eq!(
        app.commander.as_ref().unwrap().delivery_results,
        vec![format!("p{}: no longer available", id.0)]
    );
}

#[test]
fn equals_targets_send_directly_without_changing_layout_or_focus() {
    let _env = crate::persist::test_env("commander-direct-multiple");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let first = app.layout().focus;
    app.status.get_mut(&first).unwrap().agent = "zsh".into();
    app.new_tab();
    let second = app.layout().focus;
    app.status.get_mut(&second).unwrap().agent = "zsh".into();
    let (first_tx, first_rx) = std::sync::mpsc::channel();
    let (second_tx, second_rx) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&first)
        .unwrap()
        .replace_input_sender_for_test(first_tx);
    app.panes
        .get_mut(&second)
        .unwrap()
        .replace_input_sender_for_test(second_tx);
    let pane_count = app.panes.len();
    let tab_count = app.ws().tabs.len();
    app.open_commander();
    let commander = app.commander.as_mut().unwrap();
    commander.draft = format!("=p{} =p{} review this", first.0, second.0);
    commander.cursor = commander.draft.len();
    app.commander_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    for rx in [first_rx, second_rx] {
        let crate::terminal::pty::InputAction::Submit { paste, .. } = rx.try_recv().unwrap() else {
            panic!("each = target must receive one submit on the first Enter");
        };
        assert!(String::from_utf8_lossy(&paste).contains("review this"));
        assert!(rx.try_recv().is_err());
    }
    assert_eq!(app.layout().focus, second);
    assert_eq!(app.panes.len(), pane_count);
    assert_eq!(app.ws().tabs.len(), tab_count);
    assert!(app.commander.is_some());
    assert_eq!(
        app.commander.as_ref().unwrap().draft,
        format!("=p{} =p{} ", first.0, second.0)
    );
}

#[test]
fn paste_and_navigation_stay_in_composer() {
    let _env = crate::persist::test_env("commander-input");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let focused = app.layout().focus;
    app.open_commander();
    assert!(app.handle_event(AppEvent::Paste("hello 世界".into())));
    assert_eq!(
        app.commander.as_ref().unwrap().draft,
        format!("=p{} hello 世界", focused.0)
    );
    assert_eq!(app.layout().focus, focused);
    assert!(app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Backspace,
        KeyModifiers::CONTROL
    ))));
    assert_eq!(
        app.commander.as_ref().unwrap().draft,
        format!("=p{} hello ", focused.0)
    );
}

#[test]
fn prefix_navigation_keeps_the_strip_visible_and_preserves_targets() {
    let _env = crate::persist::test_env("commander-navigation");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    app.new_tab();
    app.switch_tab(0);
    app.new_workspace();
    app.cycle_workspace(-1);
    assert_eq!((app.active_ws, app.ws().active_tab), (0, 0));
    app.open_commander();
    let draft = app.commander.as_ref().unwrap().draft.clone();

    app.handle_event(AppEvent::Key(app.prefix.key_event()));
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('n'),
        KeyModifiers::NONE,
    )));
    assert_eq!((app.active_ws, app.ws().active_tab), (0, 1));
    assert!(app.commander.is_some());
    assert!(!app.commander.as_ref().unwrap().focused);

    app.handle_event(AppEvent::Key(app.prefix.key_event()));
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('u'),
        KeyModifiers::NONE,
    )));
    assert_eq!(app.active_ws, 1);
    assert_eq!(app.commander.as_ref().unwrap().draft, draft);
}

#[test]
fn clicking_the_strip_restores_editing_without_blocking_pane_paste() {
    use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let _env = crate::persist::test_env("commander-focus");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    let (input_tx, input_rx) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&pane)
        .unwrap()
        .replace_input_sender_for_test(input_tx);
    app.open_commander();
    app.commander_area = Some(Rect::new(0, 20, 80, 4));
    let draft = app.commander.as_ref().unwrap().draft.clone();

    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Esc,
        KeyModifiers::NONE,
    )));
    assert!(!app.commander.as_ref().unwrap().focused);
    assert!(app.commander.is_some());
    app.handle_event(AppEvent::Paste("pane paste".into()));
    assert!(input_rx.try_recv().is_ok());
    assert_eq!(app.commander.as_ref().unwrap().draft, draft);

    app.handle_event(AppEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 3,
        row: 21,
        modifiers: KeyModifiers::NONE,
    }));
    assert!(app.commander.as_ref().unwrap().focused);
    app.handle_event(AppEvent::Paste(" composer".into()));
    assert_eq!(
        app.commander.as_ref().unwrap().draft,
        format!("{draft} composer")
    );
    assert!(input_rx.try_recv().is_err());

    app.handle_event(AppEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 40,
        row: 10,
        modifiers: KeyModifiers::NONE,
    }));
    assert!(!app.commander.as_ref().unwrap().focused);
    assert!(app.commander.is_some());
}

#[test]
fn clicking_the_strip_in_prefix_mode_resumes_editing() {
    use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let _env = crate::persist::test_env("commander-prefix-click");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    app.new_tab();
    app.switch_tab(0);
    app.open_commander();
    app.commander_area = Some(Rect::new(0, 20, 80, 4));
    let draft = app.commander.as_ref().unwrap().draft.clone();

    app.handle_event(AppEvent::Key(app.prefix.key_event()));
    assert_eq!(app.mode, Mode::Prefix);
    assert!(!app.commander.as_ref().unwrap().focused);

    app.handle_event(AppEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 3,
        row: 21,
        modifiers: KeyModifiers::NONE,
    }));
    assert_eq!(app.mode, Mode::Normal);
    assert!(app.commander.as_ref().unwrap().focused);

    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('n'),
        KeyModifiers::NONE,
    )));
    assert_eq!(app.commander.as_ref().unwrap().draft, format!("{draft}n"));
    assert_eq!(app.ws().active_tab, 0);
}

#[test]
fn clicking_another_tab_works_while_the_strip_stays_open() {
    use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let _env = crate::persist::test_env("commander-tab-click");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    app.new_tab();
    app.switch_tab(0);
    app.open_commander();
    let draft = app.commander.as_ref().unwrap().draft.clone();
    app.commander_area = Some(Rect::new(0, 20, 80, 4));
    app.tab_rects = vec![(1, Rect::new(40, 3, 10, 1))];

    app.handle_event(AppEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 41,
        row: 3,
        modifiers: KeyModifiers::NONE,
    }));
    assert_eq!(app.ws().active_tab, 1);
    assert!(!app.commander.as_ref().unwrap().focused);
    assert_eq!(app.commander.as_ref().unwrap().draft, draft);
}
