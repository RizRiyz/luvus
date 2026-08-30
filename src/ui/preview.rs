use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::app::Selection;
use crate::files::preview::{DocumentView, LayoutKey, PreviewLoad, TextRole};
use crate::ui::theme::Theme;

use super::RenderTarget;

pub(super) fn draw(
    f: &mut RenderTarget,
    area: Rect,
    view: &DocumentView,
    selection: Option<&Selection>,
    mobile: bool,
    theme: &Theme,
) -> Vec<(String, Rect)> {
    if area.width == 0 || area.height == 0 {
        return Vec::new();
    }
    let mut links = Vec::new();
    let show_footer = !mobile || view.search.is_some();
    let body = Rect::new(
        area.x,
        area.y,
        area.width,
        area.height.saturating_sub(u16::from(show_footer)),
    );
    let key = LayoutKey {
        width: body.width.max(1),
        ascii: false,
    };
    match &view.load {
        PreviewLoad::Loading => center(f, body, "loading preview…", theme.overlay0),
        PreviewLoad::Binary(size) => center(
            f,
            body,
            &format!("binary file · {}", human(*size)),
            theme.overlay1,
        ),
        PreviewLoad::TooLarge(size) => center(
            f,
            body,
            &format!("too large to preview · {}", human(*size)),
            theme.overlay1,
        ),
        PreviewLoad::Error(error) => {
            center(f, body, &format!("cannot preview: {error}"), theme.coral)
        }
        PreviewLoad::Ready(_) => match view.layout(key) {
            Some(layout) => {
                for (screen_row, row) in layout
                    .rows
                    .iter()
                    .skip(view.scroll)
                    .take(body.height as usize)
                    .enumerate()
                {
                    let y = body.y + screen_row as u16;
                    let mut x = body.x;
                    for span in &row.spans {
                        let width = span
                            .text
                            .width()
                            .min(usize::from(body.right().saturating_sub(x)));
                        if let Some(target) = span.link.as_ref().filter(|_| width > 0) {
                            links.push((target.clone(), Rect::new(x, y, width as u16, 1)));
                        }
                        x = x.saturating_add(width as u16);
                    }
                    let line = Line::from(
                        row.spans
                            .iter()
                            .map(|span| {
                                Span::styled(span.text.clone(), role_style(span.role, theme))
                            })
                            .collect::<Vec<_>>(),
                    );
                    f.render_widget(Paragraph::new(line), Rect::new(body.x, y, body.width, 1));
                }
                draw_search(f, body, view, theme);
            }
            None if view.layout_pending(key) => center(f, body, "laying out…", theme.overlay0),
            None => center(f, body, "preparing layout…", theme.overlay0),
        },
    }

    if let Some(selection) = selection {
        let buffer = f.buffer_mut();
        for y in body.y..body.bottom() {
            for x in body.x..body.right() {
                if selection.contains(x, y) {
                    if let Some(cell) = buffer.cell_mut((x, y)) {
                        cell.set_bg(theme.sel_bg);
                    }
                }
            }
        }
    }

    if show_footer {
        let name = view
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let footer = if let Some(search) = &view.search {
            if search.editing {
                format!(" /{}", search.query)
            } else if search.matches.is_empty() {
                format!(" /{} · no matches", search.query)
            } else {
                format!(
                    " /{} · {}/{}",
                    search.query,
                    search.current + 1,
                    search.matches.len()
                )
            }
        } else {
            format!(
                " {name} · {} preview · / search · y copy source",
                view.kind.label()
            )
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                clip(&footer, area.width),
                Style::new().fg(theme.overlay0),
            ))),
            Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
        );
    }
    links
}

fn role_style(role: TextRole, theme: &Theme) -> Style {
    match role {
        TextRole::Normal => Style::new().fg(theme.text),
        TextRole::Heading(1) => Style::new().fg(theme.accent).bold(),
        TextRole::Heading(2) => Style::new().fg(theme.mint).bold(),
        TextRole::Heading(_) => Style::new().fg(theme.subtext1).bold(),
        TextRole::Emphasis => Style::new().fg(theme.text).italic(),
        TextRole::Strong => Style::new().fg(theme.text).bold(),
        TextRole::Strikethrough => Style::new().fg(theme.overlay1).crossed_out(),
        TextRole::Code => Style::new().fg(theme.amber).bg(theme.mantle),
        TextRole::Link => Style::new().fg(theme.mint).underlined(),
        TextRole::Quote => Style::new().fg(theme.overlay1),
        TextRole::Marker => Style::new().fg(theme.accent).bold(),
        TextRole::TableHeader => Style::new().fg(theme.subtext1).bold(),
        TextRole::Mermaid => Style::new().fg(theme.subtext1),
        TextRole::Muted => Style::new().fg(theme.overlay0),
        TextRole::Error => Style::new().fg(theme.coral),
    }
}

fn draw_search(f: &mut RenderTarget, body: Rect, view: &DocumentView, theme: &Theme) {
    let Some(search) = view
        .search
        .as_ref()
        .filter(|search| !search.query.is_empty())
    else {
        return;
    };
    let key = LayoutKey {
        width: body.width.max(1),
        ascii: false,
    };
    let Some(layout) = view.layout(key) else {
        return;
    };
    let buffer = f.buffer_mut();
    for (match_index, (row, byte_column)) in search.matches.iter().enumerate() {
        if *row < view.scroll || *row >= view.scroll + body.height as usize {
            continue;
        }
        let text = layout.rows[*row].plain_text();
        let start = text[..(*byte_column).min(text.len())].width();
        let end_byte = (*byte_column + search.query.len()).min(text.len());
        let width = text[*byte_column..end_byte].width().max(1);
        let background = if match_index == search.current {
            theme.accent
        } else {
            theme.amber
        };
        for column in start..start + width {
            let x = body.x.saturating_add(column as u16);
            let y = body.y + (*row - view.scroll) as u16;
            if x < body.right() {
                if let Some(cell) = buffer.cell_mut((x, y)) {
                    cell.set_bg(background);
                    cell.set_fg(theme.base);
                }
            }
        }
    }
}

fn center(f: &mut RenderTarget, area: Rect, message: &str, color: Color) {
    if area.height == 0 {
        return;
    }
    f.render_widget(
        Paragraph::new(Span::styled(
            clip(message, area.width),
            Style::new().fg(color),
        ))
        .alignment(ratatui::layout::Alignment::Center),
        Rect::new(area.x, area.y + area.height / 2, area.width, 1),
    );
}

fn clip(text: &str, width: u16) -> String {
    let mut used = 0usize;
    text.chars()
        .take_while(|ch| {
            let next = used + ch.to_string().width();
            if next > usize::from(width) {
                false
            } else {
                used = next;
                true
            }
        })
        .collect()
}

fn human(size: u64) -> String {
    if size >= 1 << 20 {
        format!("{:.1} MB", size as f64 / (1 << 20) as f64)
    } else if size >= 1 << 10 {
        format!("{:.1} KB", size as f64 / (1 << 10) as f64)
    } else {
        format!("{size} B")
    }
}
