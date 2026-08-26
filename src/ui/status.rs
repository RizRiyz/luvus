//! The bottom status line. Fixed guidance owns the left edge, Luvus Bar owns
//! the flexible middle, and the clickable version stays fixed at the right.

use super::*;

pub(super) fn draw_status(f: &mut RenderTarget, area: Rect, app: &mut App, t: &Theme) {
    if area.height == 0 {
        return;
    }
    f.render_widget(Block::new().style(Style::new().bg(t.crust)), area);
    app.version_rect = None;

    let version_text = concat!("v", env!("CARGO_PKG_VERSION"));
    let dot = if app.update_available.is_some() {
        " ●"
    } else {
        ""
    };
    let click_w = display_width(version_text).saturating_add(display_width(dot)) as u16;
    let version = if click_w < area.width {
        let rect = Rect::new(area.right().saturating_sub(click_w + 1), area.y, click_w, 1);
        app.version_rect = Some(rect);
        let hovered = app
            .hover
            .is_some_and(|(x, y)| rect.contains(Position::new(x, y)));
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    version_text,
                    Style::new().fg(if hovered { t.accent } else { t.subtext1 }),
                ),
                Span::styled(dot, Style::new().fg(t.accent).bold()),
                Span::raw(" "),
            ])),
            Rect::new(rect.x, area.y, click_w + 1, 1),
        );
        Some(rect)
    } else {
        None
    };

    let (left, show_bar) = fixed_guidance(app, t);
    let left_width = left.width() as u16;
    let left_limit = version.map_or(area.right(), |rect| rect.x);
    f.render_widget(
        Paragraph::new(left),
        Rect::new(area.x, area.y, left_limit.saturating_sub(area.x), 1),
    );

    let Some(version) = version else { return };
    if !show_bar {
        return;
    }
    const GAP: u16 = 5;
    let separator_x = version.x.saturating_sub(GAP);
    let start = area.x.saturating_add(left_width);
    let budget = separator_x
        .saturating_sub(start)
        .min(crate::bar::MAX_BAR_REGION_WIDTH);
    if budget == 0 {
        return;
    }
    let (hits, overflow, visible) = {
        let candidates =
            app.bar
                .widgets_for(crate::bar::BarRegion::BottomRight, &app.config.bars, false);
        let layout = crate::bar::compose(&candidates, budget, crate::bar::MAX_BAR_WIDGET_WIDTH);
        let visible = !layout.is_empty();
        let (hits, overflow) = crate::bar::render::draw_region(
            f,
            Rect::new(separator_x.saturating_sub(budget), area.y, budget, 1),
            crate::bar::BarRegion::BottomRight,
            &candidates,
            &layout,
            app.spinner,
            t,
        );
        (hits, overflow, visible)
    };
    app.bar.hits.extend(hits);
    if let Some(overflow) = overflow {
        app.bar.overflow_hits.push(overflow);
    }
    if visible {
        f.render_widget(
            Paragraph::new(Span::styled("  ·  ", Style::new().fg(t.overlay0))),
            Rect::new(separator_x, area.y, GAP, 1),
        );
    }
}

fn fixed_guidance(app: &App, t: &Theme) -> (Line<'static>, bool) {
    let cat = app.catalog;
    let mut left = vec![Span::raw(" ")];
    if app.scroll_pane.is_some() {
        left.push(mode_label(cat.mode_scroll, t));
        left.push(Span::raw("  "));
        left.extend(hint("1-9", cat.scroll_jump, t));
        left.extend(hint("j/k f/b ↑↓", cat.act_scroll, t));
        left.extend(hint("g/G", cat.scroll_ends, t));
        left.extend(hint("q", cat.scroll_live, t));
        return (Line::from(left), false);
    }
    if let Some(copy) = app.copy_mode {
        left.push(mode_label(cat.mode_copy, t));
        left.push(Span::raw("  "));
        // Vim's showcmd: a typed count is invisible otherwise, so `12j` looks
        // like a dead keypress until the motion lands.
        if copy.pending_count > 0 {
            left.extend(hint(&copy.pending_count.to_string(), cat.copy_count, t));
        }
        left.extend(hint("hjkl arrows", cat.act_move, t));
        left.extend(hint("v", cat.copy_anchor, t));
        left.extend(hint("y", cat.act_copy, t));
        left.extend(hint("q", cat.act_cancel, t));
        return (Line::from(left), false);
    }
    if app.mode == Mode::Resize {
        left.push(mode_label(cat.mode_resize, t));
        left.push(Span::styled(
            format!("  {}", cat.mode_resize_hint),
            Style::new().fg(t.subtext0),
        ));
        return (Line::from(left), false);
    }

    let key = |command: crate::app::Cmd| app.key_for(command);
    let prefix = app.prefix.label();
    if app.mode == Mode::Prefix {
        left.push(mode_label(cat.mode_prefix, t));
        left.push(Span::raw("  "));
        left.extend(hint("?", cat.all_keys, t));
        left.extend(hint("←↓↑→", cat.pane, t));
        left.extend(hint(
            &format!(
                "{}/{}",
                key(crate::app::Cmd::SplitRight),
                key(crate::app::Cmd::SplitDown)
            ),
            cat.act_split,
            t,
        ));
        left.extend(hint(&key(crate::app::Cmd::ClosePane), cat.act_close, t));
        left.extend(hint(&key(crate::app::Cmd::NewTab), cat.act_new_tab, t));
        left.extend(hint(
            &format!(
                "{}/{}",
                key(crate::app::Cmd::NextTab),
                key(crate::app::Cmd::PrevTab)
            ),
            cat.act_tab,
            t,
        ));
        left.extend(hint(&key(crate::app::Cmd::NewWorkspace), cat.workspace, t));
        left.extend(hint(&key(crate::app::Cmd::OpenGit), "git", t));
        left.extend(hint(&key(crate::app::Cmd::OpenBoard), "orch", t));
        left.extend(hint(&key(crate::app::Cmd::GlobalSearch), cat.act_search, t));
        return (Line::from(left), false);
    }

    left.push(Span::styled(
        format!(" {prefix} "),
        Style::new().fg(t.crust).bg(t.accent).bold(),
    ));
    left.push(Span::styled(
        format!("  {}", cat.prefix),
        Style::new().fg(t.subtext0),
    ));
    left.push(Span::styled("  ·  ", Style::new().fg(t.overlay0)));
    left.extend(hint(&format!("{prefix} ?"), cat.all_shortcuts, t));
    (Line::from(left), true)
}

fn mode_label(label: &str, t: &Theme) -> Span<'static> {
    Span::styled(
        format!(" {label} "),
        Style::new().fg(t.crust).bg(t.accent).bold(),
    )
}

fn hint(key: &str, word: &str, t: &Theme) -> Vec<Span<'static>> {
    vec![
        Span::styled(key.to_string(), Style::new().fg(t.accent).bold()),
        Span::styled(format!(" {word}   "), Style::new().fg(t.subtext0)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn row(term: &Terminal<TestBackend>, y: u16) -> String {
        let buffer = term.backend().buffer();
        (0..buffer.area.width)
            .map(|x| buffer.cell((x, y)).map_or(" ", |cell| cell.symbol()))
            .collect()
    }

    #[test]
    fn default_guidance_and_fixed_version_keep_the_existing_edges() {
        let _env = crate::persist::test_env("bar-status-default");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 30, tx).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();

        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();

        let status = row(&terminal, 29);
        let prefix = app.prefix.label();
        let guidance = format!(
            "  {prefix}   {}  ·  {prefix} ? {}",
            app.catalog.prefix, app.catalog.all_shortcuts
        );
        assert!(
            status.starts_with(&guidance),
            "unexpected guidance prefix: {status:?}"
        );
        assert!(
            status
                .trim_end()
                .ends_with(concat!("v", env!("CARGO_PKG_VERSION"))),
            "unexpected version suffix: {status:?}"
        );
        let version = app.version_rect.expect("version stays clickable");
        assert_eq!(version.right(), 119);
    }

    #[test]
    fn external_bottom_widgets_and_long_mode_hints_never_cover_version() {
        let _env = crate::persist::test_env("bar-status-fixed-lanes");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let mut segment =
            crate::bar::BarSegment::text("deploy ready", crate::bar::BarTone::Success);
        segment.action = Some("details".into());
        let widget = crate::bar::BarWidget::new(
            crate::bar::BarWidgetKey::new("example", "deploy"),
            crate::bar::BarRegion::BottomRight,
            vec![segment],
            Vec::new(),
            50,
        )
        .unwrap();
        app.bar.push_widget(widget).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();

        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();
        let version = app.version_rect.expect("version stays visible");
        assert!(app.bar.hits.iter().all(|hit| hit.rect.right() <= version.x));

        app.mode = Mode::Prefix;
        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();
        assert_eq!(app.version_rect, Some(version));
        assert!(
            app.bar.hits.is_empty(),
            "mode guidance temporarily owns the middle lane"
        );
    }

    #[test]
    fn bottom_bar_is_right_aligned_and_capped_at_100_columns() {
        let _env = crate::persist::test_env("bar-status-100");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(200, 24, tx).unwrap();
        app.config.bars.place(crate::bar::CORE_RUNTIME, None);
        let mut segment =
            crate::bar::BarSegment::text("x".repeat(100), crate::bar::BarTone::Accent);
        segment.action = Some("details".into());
        let widget = crate::bar::BarWidget::new(
            crate::bar::BarWidgetKey::new("example", "wide-bottom"),
            crate::bar::BarRegion::BottomRight,
            vec![segment],
            Vec::new(),
            50,
        )
        .unwrap();
        app.bar.push_widget(widget).unwrap();

        let area = Rect::new(0, 0, 200, 1);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        let mut target = crate::ui::RenderTarget::new(&mut buffer, area);
        let theme = app.theme.clone();
        draw_status(&mut target, area, &mut app, &theme);

        let hit = app.bar.hits.first().expect("bottom widget is visible");
        assert_eq!(hit.rect.width, crate::bar::MAX_BAR_REGION_WIDTH);
        let version = app.version_rect.expect("version remains fixed");
        assert_eq!(hit.rect.right() + 5, version.x);
    }
    /// Copy mode's guidance is clipped, never wrapped, so every hint added to the
    /// row pushes the last one off the end. Cancel has to survive: a user who
    /// cannot see `q` has no visible way out of the mode. Asserted with a count
    /// pending, which is the widest the row ever gets.
    #[test]
    fn copy_mode_guidance_keeps_its_exit_hint_at_eighty_columns() {
        let _env = crate::persist::test_env("bar-status-copy-width");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        app.copy_mode = Some(crate::app::CopyMode {
            pane,
            anchor: (0, 0),
            cursor: (0, 0),
            saved_scroll: 0,
            pending_count: 12,
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();

        let status = row(&terminal, 23);
        let cat = app.catalog;
        assert!(
            status.contains(cat.act_cancel),
            "copy mode must still show how to leave:\n{status}"
        );
        assert!(
            status.contains(cat.act_copy),
            "copy mode must still show how to copy:\n{status}"
        );
    }
}
