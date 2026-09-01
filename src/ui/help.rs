//! The keyboard-shortcut cheat-sheet overlay (`Ctrl+Space ?`): a read-only,
//! two-column list of every prefix command and every fixed shortcut, drawn last
//! over a dimmed backdrop. It scrolls with navigation keys or the mouse wheel;
//! other keys and clicks dismiss it (see `app/input.rs`).

use super::*;
use crate::app::Cmd;
use ratatui::widgets::{Borders, Clear};

pub(super) fn draw_help(f: &mut RenderTarget, area: Rect, app: &mut App, t: &Theme) {
    dim_backdrop(f, area, t);

    enum HelpRow<'a> {
        Heading(&'a str),
        Entry(String, &'a str),
    }

    let cat = app.catalog;
    let mut commands = Vec::new();
    let mut section = "";
    for &cmd in Cmd::ALL {
        let next_section = cmd.section(cat);
        if next_section != section {
            commands.push(HelpRow::Heading(next_section));
            section = next_section;
        }
        let key = app.key_for(cmd);
        commands.push(HelpRow::Entry(
            if key.is_empty() { "-".to_string() } else { key },
            cmd.label(cat),
        ));
    }

    let mut references = Vec::new();
    for (section, keys) in crate::i18n::settings::KEY_REFERENCE_KEYS.iter().enumerate() {
        references.push(HelpRow::Heading(
            cat.settings.key_reference_headings[section],
        ));
        for (row, (_canonical_key, description)) in keys
            .iter()
            .zip(cat.settings.key_reference_descriptions[section].iter())
            .enumerate()
        {
            references.push(HelpRow::Entry(
                super::settings::key_reference_label(section, row, app),
                description,
            ));
        }
    }

    let w = area.width.saturating_sub(4).clamp(54, 96).min(area.width);
    let h = area.height.saturating_sub(2).clamp(12, 32).min(area.height);
    let modal = centered_rect(area, w, h);
    f.render_widget(Clear, modal);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(t.border_focus).bg(t.surface0))
        .style(Style::new().bg(t.surface0));
    let inner = block.inner(modal);
    f.render_widget(block, modal);

    // Title — the prefix is the same for every row, so state it once.
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" {}", cat.keyboard_shortcuts),
                Style::new().fg(t.text).bold(),
            ),
            Span::styled(
                format!("   {} …", app.prefix.label()),
                Style::new().fg(t.overlay0),
            ),
        ])),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    hline(f, inner.x, inner.y + 1, inner.width, t);

    // Configurable prefix commands on the left, fixed and mode-specific
    // shortcuts on the right. Both columns share one scroll offset.
    let col_w = inner.width / 2;
    let top = inner.y + 2;
    let visible = inner.height.saturating_sub(4) as usize;
    let max_rows = commands.len().max(references.len());
    app.help_scroll_max = max_rows.saturating_sub(visible).min(usize::from(u16::MAX)) as u16;
    app.help_scroll = app.help_scroll.min(app.help_scroll_max);
    let scroll = usize::from(app.help_scroll);
    for (column, rows) in [commands.as_slice(), references.as_slice()]
        .into_iter()
        .enumerate()
    {
        let cx = inner.x + column as u16 * col_w + 1;
        for (line, row) in rows.iter().skip(scroll).take(visible).enumerate() {
            let y = top + line as u16;
            let content = match row {
                HelpRow::Heading(label) => Line::from(Span::styled(
                    label.to_uppercase(),
                    Style::new().fg(t.text).bold(),
                )),
                HelpRow::Entry(key, label) => Line::from(vec![
                    Span::styled(aligned_key(key, 13), Style::new().fg(t.accent).bold()),
                    Span::styled(*label, Style::new().fg(t.subtext1)),
                ]),
            };
            f.render_widget(
                Paragraph::new(content),
                Rect::new(cx, y, col_w.saturating_sub(2), 1),
            );
        }
    }

    // Footer.
    let footer_y = inner.bottom().saturating_sub(1);
    hline(f, inner.x, footer_y.saturating_sub(1), inner.width, t);
    f.render_widget(
        Paragraph::new(hint_line(
            &[
                ("1-9", cat.act_jump_tab),
                ("?", cat.act_this_help),
                ("↑↓ / wheel", cat.act_scroll),
                (cat.act_other_key, cat.act_close),
            ],
            t,
        )),
        Rect::new(inner.x, footer_y, inner.width, 1),
    );
}

/// Right-align a key label by terminal cells rather than Unicode scalar count.
fn aligned_key(key: &str, cells: usize) -> String {
    format!(
        "{}{key} ",
        " ".repeat(cells.saturating_sub(display_width(key)))
    )
}

// ── local render helpers (each modal module keeps its own, as elsewhere) ──

pub(super) fn centered_rect(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

pub(super) fn dim_backdrop(f: &mut RenderTarget, area: Rect, t: &Theme) {
    let buf = f.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let cell = &mut buf[(x, y)];
            cell.set_fg(t.overlay0);
            cell.set_bg(t.crust);
        }
    }
}

pub(super) fn hline(f: &mut RenderTarget, x: u16, y: u16, w: u16, t: &Theme) {
    let buf = f.buffer_mut();
    for i in 0..w {
        buf[(x + i, y)]
            .set_symbol("─")
            .set_style(Style::new().fg(t.surface1).bg(t.surface0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn screen(term: &Terminal<TestBackend>) -> String {
        term.backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn help_uses_current_bindings_and_includes_fixed_shortcuts() {
        let _env = crate::persist::test_env("complete-key-help");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(100, 32, tx).unwrap();
        app.help_open = true;
        app.prefix = crate::app::PrefixSpec::parse("f12").unwrap();
        let mut term = Terminal::new(TestBackend::new(100, 32)).unwrap();

        // The middle of the command column includes the shortcuts that were
        // easy to miss in the old clipped reference.
        app.help_scroll = 20;
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let rendered = screen(&term);
        assert!(rendered.contains("F12"), "configured prefix is shown");
        assert!(rendered.contains("Focus diff review"), "prefix+i is listed");
        assert!(rendered.contains("Files: focus"), "prefix+e is listed");

        // The final fixed-key section proves the reference is scrollable all
        // the way past command mode, Git, board, picker, and clipboard keys.
        app.help_scroll = u16::MAX;
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert_eq!(app.help_scroll, app.help_scroll_max);
        assert!(
            screen(&term).contains("MOUSE"),
            "fixed shortcuts are listed"
        );
    }

    #[test]
    fn key_alignment_uses_terminal_cell_width() {
        let ascii = aligned_key("drag", 13);
        let wide = aligned_key("拖动", 13);
        assert_eq!(display_width(&ascii), 14);
        assert_eq!(display_width(&wide), 14);
    }
}
