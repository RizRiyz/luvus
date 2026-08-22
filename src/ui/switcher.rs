//! The **switcher** / jump-palette overlay (docs/18 + docs/65): scope chips, a
//! type-to-filter line, then big finger-sized rows for tabs, workspaces, and
//! agents. Drawn last, over everything.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::{App, SwitcherRow};
use crate::ui::theme::Theme;
use crate::ui::RenderTarget;

/// Rows a tappable item occupies — two, for a comfortable touch target.
const ITEM_H: u16 = 2;

pub(super) fn draw_switcher(f: &mut RenderTarget, area: Rect, app: &mut App, t: &Theme) {
    app.switcher_rects.clear();
    app.switcher_scope_rects.clear();
    // Dim the whole screen.
    {
        let buf = f.buffer_mut();
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                if let Some(c) = buf.cell_mut((x, y)) {
                    c.set_bg(t.crust);
                }
            }
        }
    }
    let w = area.width.saturating_sub(2).min(60);
    let h = area.height.saturating_sub(2);
    let mx = area.x + (area.width.saturating_sub(w)) / 2;
    let modal = Rect::new(mx, area.y + 1, w, h);
    f.render_widget(Clear, modal);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(t.accent).bg(t.base))
        .style(Style::new().bg(t.base));
    let inner = block.inner(modal);
    f.render_widget(block, modal);

    // Title.
    f.render_widget(
        Paragraph::new(Span::styled(
            format!(" {}", app.catalog.switch_to),
            Style::new().fg(t.text).bold(),
        )),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    // Scope chips (docs/65): All | Agents | Tabs | Workspaces. The active one is
    // filled; each chip is a click target recorded for the mouse handler.
    let mut cx = inner.x + 1;
    let chip_y = inner.y + 1;
    for scope in crate::app::SwitcherScope::ALL {
        let label = format!(" {} ", scope.label(app.catalog));
        let cw = super::display_width(&label) as u16;
        if cx + cw > inner.right() {
            break;
        }
        let active = scope == app.switcher_scope;
        let style = if active {
            Style::new().fg(t.crust).bg(t.accent).bold()
        } else {
            Style::new().fg(t.subtext0)
        };
        let rect = Rect::new(cx, chip_y, cw, 1);
        f.render_widget(Paragraph::new(Span::styled(label, style)), rect);
        app.switcher_scope_rects.push((scope, rect));
        cx += cw + 1;
    }

    // Filter line (docs/65): the live query, or a dim hint when empty.
    let query_y = inner.y + 2;
    let query_line = if app.switcher_query.is_empty() {
        Line::from(Span::styled(
            format!(" {}", app.catalog.switch_filter_hint),
            Style::new().fg(t.overlay0),
        ))
    } else {
        Line::from(vec![
            Span::styled(" ", Style::new()),
            Span::styled("/ ", Style::new().fg(t.overlay0)),
            Span::styled(app.switcher_query.clone(), Style::new().fg(t.text).bold()),
            Span::styled("▏", Style::new().fg(t.accent)),
        ])
    };
    f.render_widget(
        Paragraph::new(query_line),
        Rect::new(inner.x, query_y, inner.width, 1),
    );

    let list = Rect::new(
        inner.x + 1,
        inner.y + 4,
        inner.width.saturating_sub(2),
        inner.height.saturating_sub(5),
    );

    let rows = app.switcher_rows();
    let hover = app.hover;
    let viewport = list.height as usize;

    // Document layout: assign each row a `doc_y` (cumulative visual rows) and a
    // height (header 1, item 2, action 1). This lets the list scroll when there
    // are more agents/nodes than fit a phone screen.
    let mut layout = Vec::with_capacity(rows.len());
    let mut doc_y = 0usize;
    let mut item_i = 0usize;
    let mut cursor_span: Option<(usize, usize)> = None;
    for r in &rows {
        let h = match r {
            SwitcherRow::Header(_) | SwitcherRow::Action { .. } => 1,
            _ => ITEM_H as usize,
        };
        let is_item = !matches!(r, SwitcherRow::Header(_));
        if is_item {
            if item_i == app.switcher_cursor {
                cursor_span = Some((doc_y, h));
            }
            item_i += 1;
        }
        layout.push((doc_y, h, is_item));
        doc_y += h;
    }
    let content_height = doc_y;
    let max_scroll = content_height.saturating_sub(viewport);
    // Keep the cursor in view, then clamp.
    if let Some((cy, ch)) = cursor_span {
        if cy < app.switcher_scroll {
            app.switcher_scroll = cy;
        } else if cy + ch > app.switcher_scroll + viewport {
            app.switcher_scroll = cy + ch - viewport;
        }
    }
    app.switcher_scroll = app.switcher_scroll.min(max_scroll);
    let scroll = app.switcher_scroll;

    let mut vis_item = 0usize;
    for (r, (dy, h, is_item)) in rows.iter().zip(&layout) {
        // Skip rows fully outside the scroll window.
        if *dy + *h <= scroll || *dy >= scroll + viewport {
            if *is_item {
                vis_item += 1;
            }
            continue;
        }
        let y = list.y + (*dy - scroll) as u16;
        match r {
            SwitcherRow::Header(text) => {
                f.render_widget(
                    Paragraph::new(Span::styled(
                        text.clone(),
                        Style::new().fg(t.overlay1).bold(),
                    )),
                    Rect::new(list.x, y, list.width.saturating_sub(1), 1),
                );
            }
            item => {
                let h = (*h as u16).min(list.bottom().saturating_sub(y));
                let rect = Rect::new(list.x, y, list.width.saturating_sub(1), h);
                let hovered = hover.is_some_and(|(hc, hr)| {
                    hc >= rect.x && hc < rect.right() && hr >= rect.y && hr < rect.bottom()
                });
                let selected = vis_item == app.switcher_cursor;
                if hovered || selected {
                    fill_bg(f, rect, t.sel_bg);
                }
                let (target, line1, line2) = item_lines(item, selected, t);
                f.render_widget(
                    Paragraph::new(line1),
                    Rect::new(rect.x + 1, rect.y, rect.width.saturating_sub(1), 1),
                );
                if rect.height > 1 {
                    f.render_widget(
                        Paragraph::new(line2),
                        Rect::new(rect.x + 3, rect.y + 1, rect.width.saturating_sub(3), 1),
                    );
                }
                if let Some(tg) = target {
                    app.switcher_rects.push((tg, rect));
                }
                vis_item += 1;
            }
        }
    }

    // Scrollbar on the right when the list overflows the viewport.
    if content_height > viewport {
        let track_x = inner.right().saturating_sub(1);
        let len = viewport as u16;
        let thumb = ((viewport * viewport) / content_height).max(1) as u16;
        let span = (content_height - viewport) as u16;
        let pos = if span > 0 {
            ((len.saturating_sub(thumb)) as usize * scroll / span as usize) as u16
        } else {
            0
        };
        let buf = f.buffer_mut();
        for i in 0..len {
            if let Some(c) = buf.cell_mut((track_x, list.y + i)) {
                c.set_symbol(" ");
                c.set_bg(if i >= pos && i < pos + thumb {
                    t.overlay1
                } else {
                    t.surface1
                });
            }
        }
    }

    // Footer hint.
    f.render_widget(
        Paragraph::new(Span::styled(
            format!(" {} · esc", app.catalog.act_select),
            Style::new().fg(t.overlay0),
        )),
        Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
    );
}

fn item_lines<'a>(
    item: &'a SwitcherRow,
    selected: bool,
    t: &Theme,
) -> (Option<crate::app::SwitcherTarget>, Line<'a>, Line<'a>) {
    let arrow = if selected { "▸ " } else { "  " };
    match item {
        SwitcherRow::Agent {
            target,
            state,
            title,
            location,
        } => {
            let l1 = Line::from(vec![
                Span::styled(arrow, Style::new().fg(t.accent)),
                Span::styled(format!("{} ", state.dot()), Style::new().fg(state.color(t))),
                Span::styled(title.clone(), Style::new().fg(t.text).bold()),
            ]);
            let l2 = Line::from(Span::styled(location.clone(), Style::new().fg(t.subtext0)));
            (Some(*target), l1, l2)
        }
        SwitcherRow::Tab {
            target,
            name,
            location,
            active,
        } => {
            let name_fg = if *active { t.accent } else { t.text };
            let l1 = Line::from(vec![
                Span::styled(arrow, Style::new().fg(t.accent)),
                Span::styled(name.clone(), Style::new().fg(name_fg).bold()),
            ]);
            let l2 = Line::from(Span::styled(location.clone(), Style::new().fg(t.subtext0)));
            (Some(*target), l1, l2)
        }
        SwitcherRow::Node {
            target,
            name,
            branch,
            active,
        } => {
            let name_fg = if *active { t.accent } else { t.text };
            let l1 = Line::from(vec![
                Span::styled(arrow, Style::new().fg(t.accent)),
                Span::styled(name.clone(), Style::new().fg(name_fg).bold()),
            ]);
            let l2 = Line::from(Span::styled(
                branch.clone().unwrap_or_default(),
                Style::new().fg(t.green),
            ));
            (Some(*target), l1, l2)
        }
        SwitcherRow::Action { target, label } => {
            let l1 = Line::from(vec![
                Span::styled(arrow, Style::new().fg(t.accent)),
                Span::styled(label.clone(), Style::new().fg(t.accent).bold()),
            ]);
            (Some(*target), l1, Line::default())
        }
        SwitcherRow::Header(_) => (None, Line::default(), Line::default()),
    }
}

fn fill_bg(f: &mut RenderTarget, rect: Rect, bg: ratatui::style::Color) {
    let buf = f.buffer_mut();
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            if let Some(c) = buf.cell_mut((x, y)) {
                c.set_bg(bg);
            }
        }
    }
}
