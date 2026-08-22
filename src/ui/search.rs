//! Global fuzzy-finder overlay (docs/90).

use std::collections::HashSet;

use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::App;
use crate::search::{SearchMatch, SearchScope};
use crate::ui::theme::Theme;
use crate::ui::RenderTarget;

const ITEM_HEIGHT: u16 = 1;

fn highlighted_label<'a>(result: &'a SearchMatch, base: Style, accent: Style) -> Line<'a> {
    let positions: HashSet<usize> = result.label_positions.iter().copied().collect();
    let mut spans = Vec::new();
    for (byte, ch) in result.entry.label.char_indices() {
        spans.push(Span::styled(
            ch.to_string(),
            if positions.contains(&byte) {
                accent
            } else {
                base
            },
        ));
    }
    Line::from(spans)
}

pub(super) fn draw_search(f: &mut RenderTarget, area: Rect, app: &mut App, t: &Theme) {
    {
        let buf = f.buffer_mut();
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_bg(t.crust);
                }
            }
        }
    }

    let width = area.width.saturating_sub(2).min(100);
    let height = area.height.saturating_sub(2).min(40);
    let modal = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    f.render_widget(Clear, modal);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(t.accent).bg(t.base))
        .style(Style::new().bg(t.base));
    let inner = block.inner(modal);
    f.render_widget(block, modal);

    let Some(search) = app.search.as_ref() else {
        return;
    };
    let mut scope_rects = Vec::new();
    let mut x = inner.x + 1;
    for scope in SearchScope::ALL {
        let text = format!(" {} ", scope.label());
        let width = text.chars().count() as u16;
        if x + width > inner.right() {
            break;
        }
        let style = if search.scope == scope {
            Style::new().fg(t.crust).bg(t.accent).bold()
        } else {
            Style::new().fg(t.subtext0)
        };
        let rect = Rect::new(x, inner.y, width, 1);
        f.render_widget(Paragraph::new(Span::styled(text, style)), rect);
        scope_rects.push((scope, rect));
        x += width + 1;
    }

    let case = if search.case_sensitive
        && matches!(search.scope, SearchScope::All | SearchScope::Output)
    {
        "  Aa"
    } else {
        ""
    };
    let query = if search.query.is_empty() {
        Line::from(vec![
            Span::styled(" / ", Style::new().fg(t.overlay0)),
            Span::styled("Find anything", Style::new().fg(t.overlay0)),
        ])
    } else {
        Line::from(vec![
            Span::styled(" / ", Style::new().fg(t.overlay0)),
            Span::styled(search.query.clone(), Style::new().fg(t.text).bold()),
            Span::styled("▏", Style::new().fg(t.accent)),
            Span::styled(case, Style::new().fg(t.overlay1)),
        ])
    };
    f.render_widget(
        Paragraph::new(query),
        Rect::new(inner.x, inner.y + 2, inner.width, 1),
    );

    let list_x = inner.x.saturating_add(1).min(inner.right());
    let list_y = inner.y.saturating_add(4).min(inner.bottom());
    let list_right = inner.right().saturating_sub(1).max(inner.x);
    let list_bottom = inner.bottom().saturating_sub(2).max(inner.y);
    let list = Rect::new(
        list_x,
        list_y,
        list_right.saturating_sub(list_x),
        list_bottom.saturating_sub(list_y),
    );
    let visible = (list.height / ITEM_HEIGHT) as usize;
    let scroll = if visible == 0 {
        0
    } else if search.cursor >= visible {
        search.cursor - visible + 1
    } else {
        0
    };
    let mut rects = Vec::new();
    for (visible_row, index) in (scroll..search.results.len().min(scroll + visible)).enumerate() {
        let result = &search.results[index];
        let y = list.y + visible_row as u16 * ITEM_HEIGHT;
        let rect = Rect::new(
            list.x,
            y,
            list.width,
            ITEM_HEIGHT.min(list.bottom().saturating_sub(y)),
        );
        if rect.is_empty() {
            continue;
        }
        let selected = index == search.cursor;
        let base = if selected {
            Style::new().fg(t.text).bg(t.sel_bg)
        } else {
            Style::new().fg(t.text)
        };
        let accent = if selected {
            Style::new().fg(t.accent).bg(t.sel_bg).bold()
        } else {
            Style::new().fg(t.accent).bold()
        };
        if selected {
            let buf = f.buffer_mut();
            for row in rect.y..rect.bottom() {
                for col in rect.x..rect.right() {
                    if let Some(cell) = buf.cell_mut((col, row)) {
                        cell.set_bg(t.sel_bg);
                    }
                }
            }
        }
        let kind = result.entry.kind.label();
        let kind_width = (kind.chars().count() as u16).min(rect.width);
        let gap = u16::from(rect.width > kind_width);
        let content_width = rect.width.saturating_sub(kind_width + gap);
        let mut row = Vec::new();
        row.extend(highlighted_label(result, base, accent).spans);
        row.push(Span::styled(
            format!("  {}", result.entry.detail),
            Style::new()
                .fg(t.overlay0)
                .bg(if selected { t.sel_bg } else { t.base }),
        ));
        if content_width > 0 {
            f.render_widget(
                Paragraph::new(Line::from(row)),
                Rect::new(rect.x, rect.y, content_width, 1),
            );
        }
        if kind_width > 0 {
            f.render_widget(
                Paragraph::new(Span::styled(
                    kind,
                    Style::new()
                        .fg(t.overlay1)
                        .bg(if selected { t.sel_bg } else { t.base }),
                ))
                .alignment(Alignment::Right),
                Rect::new(rect.right() - kind_width, rect.y, kind_width, 1),
            );
        }
        rects.push((index, rect));
    }

    if search.results.is_empty() && !list.is_empty() {
        let text = if search.loading {
            "  searching…"
        } else if search.query.is_empty() {
            "  no recent targets"
        } else {
            "  no fuzzy matches"
        };
        f.render_widget(
            Paragraph::new(Span::styled(text, Style::new().fg(t.overlay0))),
            Rect::new(list.x, list.y, list.width, 1),
        );
    }

    let loading = if search.loading { " · searching" } else { "" };
    let capped = if search.capped { " · partial" } else { "" };
    let footer = format!(
        " {} result{}{}{}   ↑↓ select · tab scope · ⏎ open · esc close",
        search.total,
        if search.total == 1 { "" } else { "s" },
        loading,
        capped,
    );
    f.render_widget(
        Paragraph::new(Span::styled(footer, Style::new().fg(t.overlay1))),
        Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
    );

    if let Some(search) = app.search.as_mut() {
        search.rects = rects;
        search.scope_rects = scope_rects;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::{SearchEntry, SearchKind, SearchTarget};
    use ratatui::buffer::Buffer;

    #[test]
    fn unicode_highlight_positions_preserve_the_label() {
        let result = SearchMatch {
            entry: SearchEntry::new(
                "file:0:x".into(),
                SearchKind::File,
                "Ångström".into(),
                "default › src".into(),
                [],
                SearchTarget::File {
                    ws: 0,
                    path: "x".into(),
                    workspace_cwd: ".".into(),
                },
                false,
            ),
            score: 1,
            label_positions: vec![0, 2, 3],
        };
        let line = highlighted_label(&result, Style::new(), Style::new());
        assert_eq!(
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "Ångström"
        );
    }

    #[test]
    fn results_render_as_compact_single_line_rows() {
        let _env = crate::persist::test_env("fuzzy-search-compact-rows");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(100, 30, tx).unwrap();
        app.open_search();
        let search = app.search.as_mut().unwrap();
        search.results = ["one.rs", "two.rs"]
            .into_iter()
            .enumerate()
            .map(|(index, label)| SearchMatch {
                entry: SearchEntry::new(
                    format!("file:0:{label}"),
                    SearchKind::File,
                    label.into(),
                    format!("default › src/{label}"),
                    [],
                    SearchTarget::File {
                        ws: 0,
                        path: label.into(),
                        workspace_cwd: ".".into(),
                    },
                    false,
                ),
                score: 2 - index as i64,
                label_positions: Vec::new(),
            })
            .collect();
        search.total = 2;
        search.loading = false;

        let area = Rect::new(0, 0, 100, 30);
        let mut buffer = Buffer::empty(area);
        {
            let mut target = RenderTarget::new(&mut buffer, area);
            draw_search(&mut target, area, &mut app, &Theme::quattro_rally());
        }

        let rects = &app.search.as_ref().unwrap().rects;
        assert_eq!(rects.len(), 2);
        assert!(rects.iter().all(|(_, rect)| rect.height == 1));
        assert_eq!(rects[1].1.y, rects[0].1.y + 1);
        let first = rects[0].1;
        let row: String = (first.x..first.right())
            .map(|x| buffer[(x, first.y)].symbol())
            .collect();
        assert!(
            row.ends_with("file"),
            "the kind is aligned to the right: {row:?}"
        );
        assert!(!row.contains('·'), "result kinds do not use glyph icons");
    }

    #[test]
    fn short_modal_never_places_result_rows_outside_the_overlay() {
        let _env = crate::persist::test_env("fuzzy-search-short-modal");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(12, 5, tx).unwrap();
        app.open_search();

        let area = Rect::new(0, 0, 12, 5);
        let mut buffer = Buffer::empty(area);
        {
            let mut target = RenderTarget::new(&mut buffer, area);
            draw_search(&mut target, area, &mut app, &Theme::quattro_rally());
        }

        let rects = &app.search.as_ref().unwrap().rects;
        assert!(
            rects.is_empty(),
            "a five-row terminal has no drawable result row"
        );
        assert!(rects.iter().all(|(_, rect)| {
            rect.y >= area.y
                && rect.bottom() <= area.bottom()
                && rect.x >= area.x
                && rect.right() <= area.right()
        }));
    }
}
