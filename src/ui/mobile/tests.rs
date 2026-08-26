use ratatui::layout::Rect;

use super::layout::{compute_layout, navigator_layout, resolve_profile, MobileProfile};

#[test]
fn threshold_is_inclusive_and_zero_disables_mobile() {
    assert_eq!(resolve_profile(64, 64), MobileProfile::Mobile);
    assert_eq!(resolve_profile(65, 64), MobileProfile::Desktop);
    assert_eq!(resolve_profile(34, 0), MobileProfile::Desktop);
    assert_eq!(resolve_profile(79, 80), MobileProfile::Mobile);
}

#[test]
fn mobile_header_reserves_a_compact_menu_target() {
    for (width, height) in [(79, 35), (64, 35), (44, 20), (34, 50)] {
        let layout = compute_layout(Rect::new(0, 0, width, height));
        assert_eq!(layout.header.height, 2);
        assert_eq!(layout.menu_button.width, 7);
        assert_eq!(layout.menu_button.height, 2);
        assert_eq!(layout.content.height, height - 2);
        assert_eq!(layout.content.y, 2);
    }
}

#[test]
fn navigator_close_is_compact_and_rows_use_remaining_screen() {
    let layout = navigator_layout(Rect::new(0, 0, 44, 20));
    assert_eq!(layout.close_button, Rect::new(37, 0, 7, 2));
    assert_eq!(layout.scopes, Rect::new(0, 2, 44, 1));
    assert_eq!(layout.query, Rect::new(0, 3, 44, 1));
    assert_eq!(layout.viewport, Rect::new(0, 4, 44, 16));
}

#[test]
fn mobile_sheets_use_the_full_viewport() {
    let screen = Rect::new(3, 4, 34, 20);
    assert_eq!(super::sheets::full_screen(screen), screen);
}

#[test]
fn viewport_matrix_keeps_desktop_and_mobile_geometry_distinct() {
    use ratatui::{backend::TestBackend, Terminal};

    let _env = crate::persist::test_env("mobile-viewport-matrix");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = crate::app::App::new(120, 40, tx).unwrap();
    for (width, height, mobile) in [
        (120, 40, false),
        (90, 24, false),
        (79, 35, false),
        (65, 35, false),
        (64, 35, true),
        (44, 20, true),
        (40, 60, true),
        (34, 50, true),
    ] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();
        assert_eq!(
            app.compact, mobile,
            "unexpected profile at {width}x{height}"
        );
        if mobile {
            assert_eq!(app.switcher_button_rect.unwrap().width, 7);
            assert_eq!(app.switcher_button_rect.unwrap().height, 2);
            assert_eq!(app.last_pane_area.height, height - 2);
        } else {
            assert!(app.switcher_button_rect.is_none());
        }
    }

    app.config.layout.mobile_width = 80;
    let mut termius = Terminal::new(TestBackend::new(79, 35)).unwrap();
    termius
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();
    assert!(app.compact);
    assert_eq!(app.last_pane_area.height, 33);
}

#[test]
fn mobile_navigator_uses_full_screen_and_explicit_close() {
    use ratatui::{backend::TestBackend, Terminal};

    let _env = crate::persist::test_env("mobile-full-screen-navigator");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = crate::app::App::new(64, 35, tx).unwrap();
    app.open_switcher();
    let mut terminal = Terminal::new(TestBackend::new(64, 35)).unwrap();
    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();
    assert!(app.compact);
    assert_eq!(app.switcher_close_rect.unwrap(), Rect::new(57, 0, 7, 2));
    assert!(app.switcher_rects.iter().all(|(_, rect)| rect.height == 2));
    let first = app.switcher_rects[0].1;
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer.cell((first.x, first.y)).unwrap().symbol(), "▸");
    assert_ne!(buffer.cell((first.x + 2, first.y)).unwrap().symbol(), " ");
    assert_eq!(buffer.cell((first.x, first.y + 1)).unwrap().symbol(), " ");
    assert_eq!(
        buffer.cell((first.x + 1, first.y + 1)).unwrap().symbol(),
        " "
    );
    assert_ne!(
        buffer.cell((first.x + 2, first.y + 1)).unwrap().symbol(),
        " "
    );
    assert!(app
        .switcher_rows()
        .iter()
        .any(|row| matches!(row, crate::app::SwitcherRow::Action { .. })));
    assert!(app.switcher_rows().iter().all(|row| {
        !matches!(row, crate::app::SwitcherRow::Header(label) if label == "Status")
    }));
}

#[test]
fn both_rows_of_open_menu_are_clickable() {
    use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    use ratatui::{backend::TestBackend, Terminal};

    let _env = crate::persist::test_env("mobile-two-row-open-menu");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = crate::app::App::new(64, 35, tx).unwrap();
    let mut terminal = Terminal::new(TestBackend::new(64, 35)).unwrap();
    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();
    let menu = app.switcher_button_rect.expect("open menu target");
    assert_eq!(menu.height, 2);

    assert!(app.handle_event(crate::event::AppEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: menu.x,
        row: menu.y + 1,
        modifiers: ratatui::crossterm::event::KeyModifiers::NONE,
    })));
    assert!(app.switcher, "the second MENU row opens navigation");
}

#[test]
fn mobile_settings_uses_equal_three_column_touch_grid() {
    use crate::app::SettingsTab;
    use ratatui::{backend::TestBackend, Terminal};

    let _env = crate::persist::test_env("mobile-settings-wrapped-tabs");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = crate::app::App::new(40, 35, tx).unwrap();
    app.open_settings();
    let mut terminal = Terminal::new(TestBackend::new(40, 35)).unwrap();
    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();

    assert!(app.compact);
    assert_eq!(app.settings_tab_rects.len(), SettingsTab::ALL.len());
    let rects: Vec<_> = app
        .settings_tab_rects
        .iter()
        .map(|(_, rect)| *rect)
        .collect();
    assert!(rects.iter().all(|rect| rect.width == rects[0].width));
    assert_eq!(rects[0].y, rects[1].y);
    assert_eq!(rects[1].y, rects[2].y);
    assert_eq!(rects[3].y, rects[0].y + 1);
    assert_eq!(rects[6].y, rects[0].y + 2);
    assert_eq!(rects[1].x, rects[0].right());
    assert_eq!(rects[2].x, rects[1].right());
    let language = app
        .settings_tab_rects
        .iter()
        .find(|(tab, _)| *tab == SettingsTab::Language)
        .unwrap()
        .1;
    app.handle_settings_click(language.x + 1, language.y);
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::Language);
}

#[test]
fn short_mobile_settings_stays_inside_the_viewport() {
    use ratatui::{backend::TestBackend, Terminal};

    let _env = crate::persist::test_env("mobile-settings-short-viewport");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = crate::app::App::new(24, 6, tx).unwrap();
    app.open_settings();
    let mut terminal = Terminal::new(TestBackend::new(24, 6)).unwrap();

    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();

    assert!(app.compact);
    assert_eq!(app.settings_tab_rects.len(), 1);
    assert!(app
        .settings_tab_rects
        .iter()
        .all(|(_, rect)| rect.right() <= 24 && rect.bottom() <= 6));
}

#[test]
fn mobile_replacement_surfaces_clear_underlying_symbols() {
    use ratatui::buffer::Buffer;

    let _env = crate::persist::test_env("mobile-clear-surfaces");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = crate::app::App::new(64, 35, tx).unwrap();
    app.open_switcher();
    let area = Rect::new(0, 0, 64, 35);
    let mut buffer = Buffer::empty(area);
    for cell in buffer.content.iter_mut() {
        cell.set_symbol("X");
    }
    let theme = app.theme.clone();
    let mut target = crate::ui::RenderTarget::new(&mut buffer, area);
    super::navigator::render_navigator(&mut target, area, &mut app, &theme);

    assert!(
        buffer.content.iter().all(|cell| cell.symbol() != "X"),
        "the full-screen navigator must not expose the view below it"
    );

    let header = super::layout::compute_layout(Rect::new(0, 0, 64, 2));
    for cell in buffer.content.iter_mut() {
        cell.set_symbol("X");
    }
    let mut target = crate::ui::RenderTarget::new(&mut buffer, area);
    super::header::render_header(&mut target, header, &mut app, &theme);
    assert!(
        buffer
            .content
            .iter()
            .take(64 * 2)
            .all(|cell| cell.symbol() != "X"),
        "the mobile header must clear stale desktop chrome"
    );
}

#[test]
fn mobile_header_switches_split_panes_and_wraps() {
    use ratatui::crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::{backend::TestBackend, Terminal};

    let _env = crate::persist::test_env("mobile-header-pane-switch");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = crate::app::App::new(64, 35, tx).unwrap();
    app.handle_event(crate::event::AppEvent::Key(KeyEvent::new(
        KeyCode::Char(' '),
        KeyModifiers::CONTROL,
    )));
    app.handle_event(crate::event::AppEvent::Key(KeyEvent::new(
        KeyCode::Char('v'),
        KeyModifiers::NONE,
    )));
    assert_eq!(app.layout().len(), 2);
    let leaves = app.layout().leaves();
    let first = leaves[0];
    let second = leaves[1];
    assert_eq!(app.layout().focus, second);

    let mut terminal = Terminal::new(TestBackend::new(64, 35)).unwrap();
    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();
    let previous = app.mobile_pane_prev_rect.expect("previous pane target");
    let next = app.mobile_pane_next_rect.expect("next pane target");
    assert_eq!(previous.height, 1);
    assert_eq!(next.height, 1);
    assert_eq!(previous.right(), next.x);
    assert_eq!(
        app.pane_content_rects,
        vec![(second, Rect::new(0, 2, 64, 33))]
    );

    let tap = |rect: Rect| {
        crate::event::AppEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        })
    };
    assert!(app.handle_event(tap(previous)));
    assert_eq!(app.layout().focus, first);
    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();
    assert_eq!(
        app.pane_content_rects,
        vec![(first, Rect::new(0, 2, 64, 33))]
    );

    let previous = app.mobile_pane_prev_rect.unwrap();
    assert!(app.handle_event(tap(previous)));
    assert_eq!(
        app.layout().focus,
        second,
        "previous wraps at the first pane"
    );
    let next = app.mobile_pane_next_rect.unwrap();
    assert!(app.handle_event(tap(next)));
    assert_eq!(app.layout().focus, first, "next wraps at the last pane");
}

#[test]
fn repeated_mobile_navigation_keeps_hit_storage_bounded() {
    use ratatui::{backend::TestBackend, Terminal};

    let _env = crate::persist::test_env("mobile-navigator-bounded-storage");
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = crate::app::App::new(44, 20, tx).unwrap();
    let mut terminal = Terminal::new(TestBackend::new(44, 20)).unwrap();
    for cycle in 0..1_000 {
        app.open_switcher();
        if cycle % 2 == 0 {
            app.switcher_query.push('1');
        }
        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();
        app.close_switcher();
    }
    assert!(app.switcher_rects.capacity() <= 64);
    assert!(app.switcher_scope_rects.capacity() <= 8);
}
