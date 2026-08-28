//! The tab bar: numbered button tabs with overflow scroll arrows and a `+`.

use super::*;

// ── tab bar ─────────────────────────────────────────────────────────────────

/// (tab rects, close rects, left-scroll arrow, right-scroll arrow).
pub(super) type TabHits = (
    Vec<(usize, Rect)>,
    Vec<(usize, Rect)>,
    Option<Rect>,
    Option<Rect>,
);

pub(super) fn draw_tabbar(f: &mut RenderTarget, area: Rect, app: &mut App, t: &Theme) -> TabHits {
    // Tab bar background = pane background (the sidebar is the lighter one).
    f.render_widget(Block::new().style(Style::new().bg(t.mantle)), area);

    // When the sidebar is hidden its brand `«` toggle is gone, so surface a
    // `»` (expand) at the tab-bar's left edge to bring the sidebar back. Tabs
    // start after it. (When the sidebar is shown, its header owns the toggle.)
    // The left sidebar's `«` collapse lives in its header; when it's hidden but
    // still has docks to restore, surface a `»` (expand) at the tab-bar's left
    // edge. (The right sidebar reopens via ⌃Space B or Settings.)
    app.switcher_button_rect = None;
    let left_hidden = !app.sidebars.left.visible && !app.sidebars.left.docks.is_empty();
    let tog_w = if !left_hidden {
        0
    } else {
        let r = Rect::new(area.x, area.y, 3, 1);
        let hov = app
            .hover
            .is_some_and(|(c, rr)| c >= r.x && c < r.right() && rr == r.y);
        let style = if hov {
            Style::new().fg(t.crust).bg(t.accent).bold()
        } else {
            Style::new().fg(t.accent).bg(t.surface0).bold()
        };
        f.render_widget(Paragraph::new(Span::styled(" » ", style)), r);
        app.sidebar_toggle_rect = Some(r);
        3u16
    };

    // Mirror image for the right sidebar: when it's hidden but has docks, a `«`
    // (expand) at the tab-bar's right edge brings it back (docs/29, DOCK-5).
    let right_hidden = !app.sidebars.right.visible && !app.sidebars.right.docks.is_empty();
    let right_tog_w = if !right_hidden {
        0
    } else {
        let r = Rect::new(area.right().saturating_sub(3), area.y, 3, 1);
        let hov = app
            .hover
            .is_some_and(|(c, rr)| c >= r.x && c < r.right() && rr == r.y);
        let style = if hov {
            Style::new().fg(t.crust).bg(t.accent).bold()
        } else {
            Style::new().fg(t.accent).bg(t.surface0).bold()
        };
        f.render_widget(Paragraph::new(Span::styled(" « ", style)), r);
        app.right_sidebar_toggle_rect = Some(r);
        3u16
    };

    // Preserve one active tab plus the fixed arrows/new-tab allowance, then let
    // Luvus Bar use the remaining lane up to its 100-column cap. Extra tabs use
    // the existing scroll window before bar content is compressed.
    const ARROW: u16 = 2;
    const PLUS: u16 = 3;
    const NAV_RESERVE: u16 = PLUS + 2 * ARROW;
    let fixed = tog_w
        .saturating_add(right_tog_w)
        .saturating_add(NAV_RESERVE);
    let flex = area.width.saturating_sub(fixed);
    let top_budget = flex
        .saturating_sub(crate::bar::MIN_TOP_TAB_FLEX_WIDTH)
        .min(crate::bar::MAX_BAR_REGION_WIDTH);
    let (bar_hits, bar_overflow, bar_w) = {
        let candidates =
            app.bar
                .widgets_for(crate::bar::BarRegion::TopRight, &app.config.bars, false);
        let layout = crate::bar::compose(&candidates, top_budget, crate::bar::MAX_BAR_WIDGET_WIDTH);
        let width = layout.width;
        let region = Rect::new(
            area.right().saturating_sub(right_tog_w + width),
            area.y,
            width,
            1,
        );
        let (hits, overflow) = crate::bar::render::draw_region(
            f,
            region,
            crate::bar::BarRegion::TopRight,
            &candidates,
            &layout,
            app.spinner,
            t,
        );
        (hits, overflow, width)
    };
    app.bar.hits.extend(bar_hits);
    if let Some(overflow) = bar_overflow {
        app.bar.overflow_hits.push(overflow);
    }

    let ws = app.ws();
    let n = ws.tabs.len();
    let active = ws.active_tab;
    let mut tab_rects = Vec::new();
    let mut close_rects = Vec::new();
    let mut prev_rect = None;
    let mut next_rect = None;

    // Tabs are *variable* width: each is as wide as its label needs, never
    // narrower than `MIN` (the old fixed cell, so a bar of numbered tabs looks
    // exactly as it always did) and never wider than `max_w`. Every tab reserves
    // the same trailing `✕` columns whether or not it is active, so a label
    // never shifts sideways when you focus its tab.
    const MIN: u16 = 10; // the old fixed cell, now the floor
    const MAX: u16 = 28; // ceiling, so one long name can't eat the whole bar
    const PAD: u16 = 2; // one blank column each side of the label
    const CLOSE: u16 = 2; // the `✕ ` slot, reserved on every tab
    const GAP: u16 = 1;
    let left = area.x + 1 + tog_w;
    let right = area
        .right()
        .saturating_sub(right_tog_w + bar_w + if bar_w > 0 { 1 } else { 0 });
    let total = right.saturating_sub(left);

    // No single tab takes more than a third of the strip, so a long name still
    // leaves its neighbours legible on a narrow terminal.
    let max_w = MAX.min((total / 3).max(MIN));
    let labels: Vec<String> = (0..n).map(|i| tab_label(ws, app, i)).collect();
    let widths: Vec<u16> = labels
        .iter()
        .map(|l| {
            (display_width(l) as u16)
                .saturating_add(PAD + CLOSE)
                .clamp(MIN, max_w)
        })
        .collect();
    let strip = |a: usize, b: usize| -> u16 {
        widths[a..b].iter().sum::<u16>() + (b - a).saturating_sub(1) as u16 * GAP
    };

    // Do all tabs fit without scroll arrows (leaving room for the "+")?
    let need_scroll = strip(0, n) > total.saturating_sub(PLUS);
    let avail = if need_scroll {
        total.saturating_sub(PLUS + 2 * ARROW)
    } else {
        total.saturating_sub(PLUS)
    };
    // Scroll the window so the active tab stays visible: pack leftward from the
    // active tab, then spend whatever room is left extending to the right. With
    // uniform widths this lands on the same window the old fixed-cell math did.
    let mut scroll = active.min(n.saturating_sub(1));
    let mut end = (active + 1).min(n);
    let mut used = widths.get(active).copied().unwrap_or(0);
    while scroll > 0 && used + widths[scroll - 1] + GAP <= avail {
        used += widths[scroll - 1] + GAP;
        scroll -= 1;
    }
    while end < n && used + widths[end] + GAP <= avail {
        used += widths[end] + GAP;
        end += 1;
    }

    let mut x = left;
    // Left scroll arrow.
    if need_scroll {
        let style = if scroll > 0 {
            Style::new().fg(t.accent).bold()
        } else {
            Style::new().fg(t.overlay0)
        };
        let r = Rect::new(x, area.y, ARROW, 1);
        f.render_widget(Paragraph::new(Span::styled("‹ ", style)), r);
        prev_rect = Some(r);
        x += ARROW;
    }

    for i in scroll..end {
        // Clamp to the room actually left, so a bar too narrow for even one tab
        // clips instead of drawing over the arrows and the `+`.
        let w = widths[i].min(right.saturating_sub(x));
        if w <= CLOSE {
            break;
        }
        // The label is centered over everything but the `✕` slot, which leaves a
        // blank column on each side of the text at the tab's floor width and more
        // as the name grows — the text never touches an edge, and never the `✕`.
        let text_w = (w - CLOSE) as usize;
        let label = center(&truncate(&labels[i], text_w.saturating_sub(2)), text_w);
        let rect = Rect::new(x, area.y, w, 1);
        let style = if i == active {
            Style::new().fg(t.crust).bg(t.accent).bold()
        } else {
            // Inactive tab: same as the pane background.
            Style::new().fg(t.subtext0).bg(t.mantle)
        };
        // Paint the whole tab first: the label widget covers all but the reserved
        // `✕` columns, which stay the tab's own colour on an inactive tab.
        f.render_widget(Block::new().style(style), rect);
        f.render_widget(
            Paragraph::new(Span::styled(label, style)),
            Rect::new(x, area.y, w - CLOSE, 1),
        );
        if i == active {
            // The active tab keeps its `✕` close button in the reserved columns.
            let close = Rect::new(x + w - CLOSE, area.y, CLOSE, 1);
            f.render_widget(
                Paragraph::new(Span::styled("✕ ", Style::new().fg(t.crust).bg(t.accent))),
                close,
            );
            close_rects.push((i, close));
        }
        tab_rects.push((i, rect));
        x += w + GAP;
    }

    // Right scroll arrow.
    if need_scroll {
        let style = if end < n {
            Style::new().fg(t.accent).bold()
        } else {
            Style::new().fg(t.overlay0)
        };
        let r = Rect::new(x, area.y, ARROW, 1);
        f.render_widget(Paragraph::new(Span::styled("› ", style)), r);
        next_rect = Some(r);
        x += ARROW;
    }

    // "+" new-tab button (clickable; index == tab count).
    if x + PLUS <= right {
        let rect = Rect::new(x, area.y, PLUS, 1);
        f.render_widget(
            Paragraph::new(Span::styled(
                " + ",
                Style::new().fg(t.accent).bg(t.surface0).bold(),
            )),
            rect,
        );
        tab_rects.push((n, rect));
    }
    (tab_rects, close_rects, prev_rect, next_rect)
}

/// The text a tab shows, before any padding — what its width is measured from.
///
/// A git tab is labeled `⎇ git`, the orchestration board `◇ orch`, Mission
/// Control `⦿ ctrl`; a user-named pane tab
/// (docs/28) shows its name, a single file-view leaf (docs/38) `■ name`, and
/// everything else its number.
fn tab_label(ws: &crate::app::Workspace, app: &App, i: usize) -> String {
    let Some(tb) = ws.tabs.get(i) else {
        return String::new();
    };
    if let Some(nm) = tb.name.as_deref() {
        nm.to_string()
    } else if let Some(fl) = file_tab_name(tb, app) {
        fl
    } else if tb.is_git() {
        "⎇ git".to_string()
    } else if tb.is_orch() {
        "◇ orch".to_string()
    } else if tb.is_mission() {
        "⦿ ctrl".to_string()
    } else {
        (i + 1).to_string()
    }
}

/// Center `s` in `w` columns, measured in display width so a CJK or emoji label
/// sits square rather than drifting a cell per wide glyph (docs/21).
fn center(s: &str, w: usize) -> String {
    let sw = display_width(s);
    if sw >= w {
        return s.to_string();
    }
    let l = (w - sw) / 2;
    format!("{}{}{}", " ".repeat(l), s, " ".repeat(w - sw - l))
}

/// If `tab` shows a single file (docs/38), its `■ name` label (a plain square
/// glyph, no emoji).
///
/// Both ways of opening a file land here, so the two look identical in the tab
/// bar: the read-only viewer (a native view leaf) and a terminal editor (a real
/// PTY pane tracked in `editor_files`).
fn file_tab_name(tab: &crate::app::Tab, app: &App) -> Option<String> {
    let leaves = tab.layout.leaves();
    if leaves.len() != 1 {
        return None;
    }
    let id = leaves[0];
    match app.views.get(&id) {
        Some(crate::app::ViewKind::File(v)) => {
            let name = v.path.file_name()?.to_string_lossy().into_owned();
            Some(format!("■ {name}"))
        }
        Some(crate::app::ViewKind::Diff(v)) => Some(format!(
            "{} {}",
            crate::diff::DIFF_GLYPH,
            v.key.display_path()
        )),
        None => {
            let name = app
                .editor_files
                .get(&id)?
                .path
                .file_name()?
                .to_string_lossy()
                .into_owned();
            Some(format!("■ {name}"))
        }
    }
}

#[cfg(test)]
mod width_tests {
    use crate::app::App;
    use crate::event::AppEvent;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::layout::Rect;
    use ratatui::Terminal;

    /// The drawn text of a tab, taken straight out of the rendered buffer.
    fn drawn(term: &Terminal<TestBackend>, r: Rect) -> String {
        let buf = term.backend().buffer();
        (r.x..r.right())
            .map(|c| buf.cell((c, r.y)).map(|x| x.symbol()).unwrap_or(" "))
            .collect()
    }

    fn rect_of(app: &App, i: usize) -> Rect {
        app.tab_rects
            .iter()
            .find(|(n, _)| *n == i)
            .map(|(_, r)| *r)
            .unwrap_or_else(|| panic!("tab {i} was not drawn"))
    }

    /// The point of variable-width tabs: a long name is shown *whole* instead of
    /// being cut to the fixed cell, and the tab grows to hold it. A short name
    /// still gets the old fixed width, so an ordinary bar looks unchanged.
    #[test]
    fn a_long_name_widens_its_tab_and_shows_in_full() {
        let _env = crate::persist::test_env("tab-wide");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(160, 40, tx).unwrap();
        let mut term = Terminal::new(TestBackend::new(160, 40)).unwrap();

        app.run_cmd(crate::app::Cmd::NewTab);
        let long = "refactor-detect";
        app.workspaces[0].tabs[1].name = Some(long.to_string());
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();

        let short = rect_of(&app, 0);
        let wide = rect_of(&app, 1);
        assert_eq!(short.width, 10, "a numbered tab keeps the old fixed width");
        assert!(
            wide.width > short.width,
            "the named tab grew: {} vs {}",
            wide.width,
            short.width
        );
        let text = drawn(&term, wide);
        assert!(
            text.contains(long),
            "the whole name is drawn, not an ellipsis: {text:?}"
        );
        // Padding on both sides, and the `✕` still has its slot.
        assert!(text.starts_with(' '), "space at the left edge: {text:?}");
        assert!(text.ends_with("✕ "), "the close button is kept: {text:?}");
        assert!(
            text.trim_end_matches("✕ ").ends_with(' '),
            "space between the name and the ✕: {text:?}"
        );
    }

    /// A name past the cap has to be cut — but it is cut to leave the padding and
    /// the `✕` intact, so text never runs into either edge.
    #[test]
    fn an_over_long_name_is_capped_but_keeps_its_padding_and_close_button() {
        let _env = crate::persist::test_env("tab-cap");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(160, 40, tx).unwrap();
        let mut term = Terminal::new(TestBackend::new(160, 40)).unwrap();

        app.workspaces[0].tabs[0].name = Some("a-really-very-long-tab-name-that-runs-on".into());
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();

        let r = rect_of(&app, 0);
        assert!(r.width <= 28, "capped at the ceiling, got {}", r.width);
        let text = drawn(&term, r);
        assert!(text.contains('…'), "cut with an ellipsis: {text:?}");
        assert!(text.starts_with(' '), "space at the left edge: {text:?}");
        assert!(text.ends_with("✕ "), "the close button survives: {text:?}");
        assert!(
            text.trim_end_matches("✕ ").ends_with(' '),
            "space before the ✕: {text:?}"
        );
    }

    /// The `✕` columns are reserved on *every* tab, not just the focused one, so
    /// focusing a tab can't resize it or shove its label sideways — the bar would
    /// visibly reflow under the pointer otherwise.
    #[test]
    fn focusing_a_tab_does_not_move_or_resize_it() {
        let _env = crate::persist::test_env("tab-stable");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(160, 40, tx).unwrap();
        let mut term = Terminal::new(TestBackend::new(160, 40)).unwrap();

        app.run_cmd(crate::app::Cmd::NewTab);
        app.workspaces[0].tabs[1].name = Some("build".into());
        app.workspaces[0].active_tab = 0;
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let (inactive_rect, inactive_text) = (rect_of(&app, 1), drawn(&term, rect_of(&app, 1)));

        app.workspaces[0].active_tab = 1;
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let active_rect = rect_of(&app, 1);

        assert_eq!(inactive_rect, active_rect, "same rect either way");
        let active_text = drawn(&term, active_rect);
        assert_eq!(
            inactive_text.trim_end(),
            active_text.trim_end_matches("✕ ").trim_end(),
            "the label sits in the same columns either way"
        );
    }

    /// With every tab the same width this must pick the same window the old
    /// fixed-cell arithmetic did: the active tab visible, packed from the left.
    #[test]
    fn many_tabs_still_scroll_to_keep_the_active_one_visible() {
        let _env = crate::persist::test_env("tab-scroll");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(100, 40, tx).unwrap();
        let mut term = Terminal::new(TestBackend::new(100, 40)).unwrap();

        for _ in 0..12 {
            app.run_cmd(crate::app::Cmd::NewTab);
        }
        for active in [0usize, 6, 12] {
            app.workspaces[0].active_tab = active;
            term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
            assert!(
                app.tab_rects.iter().any(|(i, _)| *i == active),
                "tab {active} is on screen"
            );
            // Nothing spills past the bar.
            for (_, r) in &app.tab_rects {
                assert!(r.right() <= 100, "tab drawn inside the bar: {r:?}");
            }
        }
    }

    #[test]
    fn a_120_column_tab_row_shows_100_bar_columns_and_keeps_navigation() {
        let _env = crate::persist::test_env("tab-bar-100");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        let widget = crate::bar::BarWidget::new(
            crate::bar::BarWidgetKey::new("example", "wide"),
            crate::bar::BarRegion::TopRight,
            vec![crate::bar::BarSegment::text(
                "x".repeat(100),
                crate::bar::BarTone::Accent,
            )],
            Vec::new(),
            50,
        )
        .unwrap();
        app.bar.push_widget(widget).unwrap();

        let area = Rect::new(0, 0, 120, 1);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        let mut target = crate::ui::RenderTarget::new(&mut buffer, area);
        let theme = app.theme.clone();
        let (tabs, _, _, _) = super::draw_tabbar(&mut target, area, &mut app, &theme);

        let rendered: String = (20..120)
            .map(|x| buffer.cell((x, 0)).map_or(" ", |cell| cell.symbol()))
            .collect();
        assert_eq!(rendered, "x".repeat(100));
        assert!(
            tabs.iter().any(|(index, _)| *index == 0),
            "active tab remains"
        );
        assert!(
            tabs.iter().any(|(index, _)| *index == 1),
            "new-tab button remains"
        );
    }

    #[test]
    fn overflow_arrows_move_the_active_tab_in_both_directions_with_a_wide_bar() {
        let _env = crate::persist::test_env("tab-bar-arrow-clicks");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(180, 40, tx).unwrap();
        for _ in 0..27 {
            app.workspaces[0].tabs.push(crate::app::Tab {
                id: crate::ids::public_id("tab"),
                layout: crate::layout::TileLayout::new(crate::ids::PaneId::alloc()),
                git: None,
                orch: false,
                mission: false,
                name: None,
            });
        }
        app.workspaces[0].active_tab = 27;
        assert_eq!(app.workspaces[0].tabs.len(), 28, "test has 28 tabs");
        let widget = crate::bar::BarWidget::new(
            crate::bar::BarWidgetKey::new("example", "wide"),
            crate::bar::BarRegion::TopRight,
            vec![crate::bar::BarSegment::text(
                "x".repeat(100),
                crate::bar::BarTone::Accent,
            )],
            Vec::new(),
            50,
        )
        .unwrap();
        app.bar.push_widget(widget).unwrap();

        let mut term = Terminal::new(TestBackend::new(180, 40)).unwrap();
        term.draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();
        let left = app.tab_prev_rect.expect("left overflow arrow");
        assert!(app.handle_event(AppEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: left.x,
            row: left.y,
            modifiers: KeyModifiers::NONE,
        })));
        assert_eq!(
            app.workspaces[0].active_tab, 26,
            "left arrow selects tab 27"
        );

        term.draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();
        let right = app.tab_next_rect.expect("right overflow arrow");
        assert!(app.handle_event(AppEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: right.x,
            row: right.y,
            modifiers: KeyModifiers::NONE,
        })));
        assert_eq!(
            app.workspaces[0].active_tab, 27,
            "right arrow selects tab 28"
        );
    }
}

#[cfg(test)]
mod close_button_tests {
    use crate::app::App;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn mission_control_uses_the_bullseye_tab_icon() {
        let _env = crate::persist::test_env("mission-tab-icon");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        app.open_mission_control(app.active_ws);

        let active = app.ws().active_tab;
        assert_eq!(super::tab_label(app.ws(), &app, active), "⦿ ctrl");
        assert_eq!(unicode_width::UnicodeWidthStr::width("⦿"), 1);
    }

    /// The dashboard tabs are *views*, not pane trees, but they are still tabs a
    /// user opens and wants gone — so the active one must carry the same `✕` a
    /// pane tab does, and clicking it must actually remove the tab.
    #[test]
    fn dashboard_tabs_have_a_working_close_button() {
        let _env = crate::persist::test_env("dash-close");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();

        for label in ["orch", "mission"] {
            let before = app.ws().tabs.len();
            if label == "orch" {
                app.open_orch_board();
            } else {
                app.open_mission_control(app.active_ws);
            }
            assert_eq!(app.ws().tabs.len(), before + 1, "{label} tab opened");
            term.draw(|f| crate::ui::render(f, &mut app)).unwrap();

            let active = app.ws().active_tab;
            let close = app
                .tab_close_rects
                .iter()
                .find(|(i, _)| *i == active)
                .map(|(_, r)| *r)
                .unwrap_or_else(|| panic!("{label} tab has no close button"));

            app.handle_event(crate::event::AppEvent::Mouse(
                ratatui::crossterm::event::MouseEvent {
                    kind: ratatui::crossterm::event::MouseEventKind::Down(
                        ratatui::crossterm::event::MouseButton::Left,
                    ),
                    column: close.x,
                    row: close.y,
                    modifiers: ratatui::crossterm::event::KeyModifiers::NONE,
                },
            ));
            assert_eq!(app.ws().tabs.len(), before, "{label} tab closed by its ✕");
        }
    }
}
