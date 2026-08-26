use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::app::{App, SwitcherRow, SwitcherScope, SwitcherTarget};
use crate::ui::theme::Theme;
use crate::ui::{display_width, truncate, RenderTarget};

use super::layout::navigator_layout;

const ITEM_HEIGHT: usize = 2;

pub(crate) fn render_navigator(f: &mut RenderTarget, area: Rect, app: &mut App, t: &Theme) {
    let geometry = navigator_layout(area);
    // The navigator is a replacement surface, not a translucent overlay. A
    // styled Block changes colors but deliberately preserves cell symbols,
    // which would leave terminal/Git text visible through every unused cell.
    f.render_widget(Clear, area);
    f.render_widget(Block::new().style(Style::new().bg(t.base)), area);
    app.switcher_rects.clear();
    app.switcher_scope_rects.clear();
    app.switcher_close_rect = Some(geometry.close_button);

    f.render_widget(
        Paragraph::new(Span::styled(
            format!(" {}", app.catalog.switch_to.to_uppercase()),
            Style::new().fg(t.text).bold(),
        )),
        geometry.header,
    );
    let close_hovered = app.hover.is_some_and(|(x, y)| {
        x >= geometry.close_button.x
            && x < geometry.close_button.right()
            && y >= geometry.close_button.y
            && y < geometry.close_button.bottom()
    });
    let (close_fg, close_bg) = if close_hovered {
        (t.crust, t.accent)
    } else {
        (t.accent, t.surface0)
    };
    f.render_widget(
        Block::new().style(Style::new().bg(close_bg)),
        geometry.close_button,
    );
    f.render_widget(
        Paragraph::new(vec![
            Line::from(app.catalog.act_close.to_uppercase()),
            Line::from(app.catalog.menu.to_uppercase()),
        ])
        .alignment(ratatui::layout::Alignment::Center)
        .style(Style::new().fg(close_fg).bg(close_bg).bold()),
        geometry.close_button,
    );

    if geometry.scopes.height > 0 {
        let mut x = geometry.scopes.x + 1;
        for scope in SwitcherScope::ALL {
            let label = format!(" {} ", scope.label(app.catalog));
            let width = display_width(&label) as u16;
            if x.saturating_add(width) > geometry.scopes.right() {
                break;
            }
            let rect = Rect::new(x, geometry.scopes.y, width, 1);
            let style = if scope == app.switcher_scope {
                Style::new().fg(t.crust).bg(t.accent).bold()
            } else {
                Style::new().fg(t.subtext0)
            };
            f.render_widget(Paragraph::new(Span::styled(label, style)), rect);
            app.switcher_scope_rects.push((scope, rect));
            x = x.saturating_add(width + 1);
        }
    }

    if geometry.query.height > 0 {
        let query = if app.switcher_query.is_empty() {
            Span::styled(
                format!(" / {}", app.catalog.switch_filter_hint),
                Style::new().fg(t.overlay0),
            )
        } else {
            Span::styled(
                format!(" / {}\u{258f}", app.switcher_query),
                Style::new().fg(t.text).bold(),
            )
        };
        f.render_widget(Paragraph::new(query), geometry.query);
    }

    let rows = app.switcher_rows();
    let viewport = geometry.viewport.height as usize;
    let mut document = Vec::with_capacity(rows.len());
    let mut document_y = 0usize;
    let mut item_index = 0usize;
    let mut cursor_span = None;
    for row in &rows {
        let height = if matches!(row, SwitcherRow::Header(_)) {
            1
        } else {
            ITEM_HEIGHT
        };
        let target = row_target(row);
        if target.is_some() {
            if item_index == app.switcher_cursor {
                cursor_span = Some((document_y, height));
            }
            item_index += 1;
        }
        document.push((document_y, height, target));
        document_y += height;
    }
    let max_scroll = document_y.saturating_sub(viewport);
    if let Some((top, height)) = cursor_span {
        if top < app.switcher_scroll {
            app.switcher_scroll = top;
        } else if top + height > app.switcher_scroll + viewport {
            app.switcher_scroll = top + height - viewport;
        }
    }
    app.switcher_scroll = app.switcher_scroll.min(max_scroll);
    let scroll = app.switcher_scroll;

    let hover = app.hover;
    let mut current_item = 0usize;
    for (row, (top, height, target)) in rows.iter().zip(document.iter()) {
        if *top + *height <= scroll || *top >= scroll + viewport {
            if target.is_some() {
                current_item += 1;
            }
            continue;
        }
        let clipped_top = (*top).max(scroll);
        let clipped_bottom = (*top + *height).min(scroll + viewport);
        let y = geometry.viewport.y + (clipped_top - scroll) as u16;
        let visible_height = (clipped_bottom - clipped_top) as u16;
        let width = geometry
            .viewport
            .width
            .saturating_sub(u16::from(document_y > viewport));
        let rect = Rect::new(geometry.viewport.x, y, width, visible_height);
        match row {
            SwitcherRow::Header(label) => {
                let label = format!("{} ", label.to_lowercase());
                let remaining = rect.width.saturating_sub(display_width(&label) as u16);
                f.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled(label, Style::new().fg(t.overlay1).bold()),
                        Span::styled("─".repeat(remaining as usize), Style::new().fg(t.surface1)),
                    ])),
                    rect,
                );
            }
            _ => {
                let selected = current_item == app.switcher_cursor;
                let hovered = hover.is_some_and(|(x, y)| {
                    x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom()
                });
                if selected || hovered {
                    fill(f, rect, t.sel_bg);
                }
                render_item(f, rect, row, selected, t);
                if let Some(target) = target {
                    // Partially visible entries are deliberately not clickable.
                    if visible_height as usize == *height {
                        app.switcher_rects.push((*target, rect));
                    }
                    current_item += 1;
                }
            }
        }
    }

    if document_y > viewport {
        if let Some(track) = geometry.scrollbar {
            let thumb_height = ((viewport * viewport) / document_y).max(1) as u16;
            let travel = track.height.saturating_sub(thumb_height);
            let position = (travel as usize * scroll)
                .checked_div(max_scroll)
                .unwrap_or(0) as u16;
            for offset in 0..track.height {
                let color = if offset >= position && offset < position + thumb_height {
                    t.overlay1
                } else {
                    t.surface1
                };
                fill(f, Rect::new(track.x, track.y + offset, 1, 1), color);
            }
        }
    }
}

fn row_target(row: &SwitcherRow) -> Option<SwitcherTarget> {
    match row {
        SwitcherRow::Agent { target, .. }
        | SwitcherRow::Tab { target, .. }
        | SwitcherRow::Node { target, .. }
        | SwitcherRow::Action { target, .. } => Some(*target),
        SwitcherRow::Header(_) => None,
    }
}

fn render_item(f: &mut RenderTarget, rect: Rect, row: &SwitcherRow, selected: bool, t: &Theme) {
    let arrow = if selected { "▸" } else { " " };
    let width = rect.width as usize;
    let prefix = Span::styled(format!("{arrow} "), Style::new().fg(t.accent));
    let (primary, secondary) = match row {
        SwitcherRow::Agent {
            state,
            title,
            location,
            ..
        } => (
            Line::from(vec![
                prefix,
                Span::styled(
                    pad_right(&state.label().to_uppercase(), 10),
                    Style::new().fg(state.color(t)),
                ),
                Span::styled(format!("{} ", state.dot()), Style::new().fg(state.color(t))),
                Span::styled(
                    truncate(title, width.saturating_sub(14)),
                    Style::new().fg(t.text).bold(),
                ),
            ]),
            detail_line(location, width, t),
        ),
        SwitcherRow::Tab {
            name,
            location,
            active,
            ..
        } => (
            Line::from(vec![
                prefix,
                Span::styled(
                    truncate(name, width.saturating_sub(4)),
                    Style::new()
                        .fg(if *active { t.accent } else { t.text })
                        .bold(),
                ),
                Span::styled(if *active { " ●" } else { "" }, Style::new().fg(t.accent)),
            ]),
            detail_line(location, width, t),
        ),
        SwitcherRow::Node {
            name,
            branch,
            active,
            ..
        } => (
            Line::from(vec![
                prefix,
                Span::styled(
                    truncate(name, width.saturating_sub(4)),
                    Style::new()
                        .fg(if *active { t.accent } else { t.text })
                        .bold(),
                ),
                Span::styled(if *active { " ●" } else { "" }, Style::new().fg(t.accent)),
            ]),
            detail_line(branch.as_deref().unwrap_or(""), width, t),
        ),
        SwitcherRow::Action { label, detail, .. } => (
            Line::from(vec![
                prefix,
                Span::styled(
                    truncate(label, width.saturating_sub(2)),
                    Style::new().fg(t.accent).bold(),
                ),
            ]),
            detail_line(detail, width, t),
        ),
        SwitcherRow::Header(_) => return,
    };
    f.render_widget(
        Paragraph::new(primary),
        Rect::new(rect.x, rect.y, rect.width, 1),
    );
    if rect.height > 1 {
        f.render_widget(
            Paragraph::new(secondary),
            Rect::new(rect.x, rect.y + 1, rect.width, 1),
        );
    }
}

fn detail_line<'a>(detail: &'a str, width: usize, t: &Theme) -> Line<'a> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            truncate(detail, width.saturating_sub(2)),
            Style::new().fg(t.subtext0),
        ),
    ])
}

fn pad_right(text: &str, width: usize) -> String {
    let text = truncate(text, width);
    let padding = width.saturating_sub(display_width(&text));
    format!("{text}{}", " ".repeat(padding))
}

fn fill(f: &mut RenderTarget, rect: Rect, color: ratatui::style::Color) {
    let buffer = f.buffer_mut();
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            if let Some(cell) = buffer.cell_mut((x, y)) {
                cell.set_bg(color);
            }
        }
    }
}
