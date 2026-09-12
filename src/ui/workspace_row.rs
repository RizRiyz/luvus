//! Shared native workspace presentation for server and machine-shell docks.
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
};

#[derive(Clone, Copy)]
pub(crate) struct Palette {
    pub accent: Color,
    pub normal: Color,
    pub muted: Color,
    pub path: Color,
    pub branch: Color,
    pub active_bg: Color,
    pub selected_bg: Color,
}

pub(crate) struct WorkspaceRow<'a> {
    pub name: &'a str,
    pub branch: Option<&'a str>,
    /// Home abbreviation is performed by the owning server, never the client.
    pub path: &'a str,
    pub dot: &'a str,
    pub dot_color: Color,
    pub nested: bool,
    pub active: bool,
    pub selected: bool,
    pub hovered: bool,
}

pub(crate) fn draw(buf: &mut Buffer, area: Rect, row: WorkspaceRow<'_>, t: Palette) {
    if area.width < 3 || area.height == 0 {
        return;
    }
    let cx = area.x + 2;
    // Leave the native scrollbar column intact, including on narrow docks.
    let cw = area.width.saturating_sub(4);
    let indent = usize::from(row.nested) * 2;
    let avail = usize::from(cw).saturating_sub(indent + 2);
    let name_width = super::display_width(row.name);
    let (name, branch) = match row.branch {
        Some(branch) if name_width + 2 + super::display_width(branch) <= avail => {
            (row.name.to_string(), Some(branch.to_string()))
        }
        Some(branch) if name_width + 4 <= avail => (
            row.name.to_string(),
            Some(super::truncate(branch, avail - name_width - 2)),
        ),
        _ => (super::truncate(row.name, avail), None),
    };
    let mut spans = Vec::with_capacity(5);
    if row.nested {
        spans.push(Span::styled("└ ", Style::new().fg(t.muted)));
    }
    spans.push(Span::styled(
        row.dot.to_string(),
        Style::new().fg(row.dot_color),
    ));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        name,
        if row.active || row.selected {
            Style::new().fg(t.accent).bold()
        } else {
            Style::new().fg(t.normal)
        },
    ));
    if let Some(branch) = branch {
        spans.push(Span::styled(
            format!("  {branch}"),
            Style::new().fg(if row.selected {
                t.accent
            } else if row.active {
                t.branch
            } else {
                t.muted
            }),
        ));
    }
    buf.set_line(cx, area.y, &Line::from(spans), cw);
    if area.height > 1 {
        let pad = 2 + indent;
        let path = path_tail(row.path, usize::from(cw).saturating_sub(pad));
        buf.set_line(
            cx,
            area.y + 1,
            &Line::from(Span::styled(
                format!("{}{path}", " ".repeat(pad)),
                Style::new().fg(if row.selected {
                    t.accent
                } else if row.active {
                    t.path
                } else {
                    t.muted
                }),
            )),
            cw,
        );
    }
    if row.active || row.selected || row.hovered {
        for y in area.y..area.bottom() {
            for x in area.x..area.right().saturating_sub(1) {
                buf[(x, y)].set_bg(if row.selected || row.hovered {
                    t.selected_bg
                } else {
                    t.active_bg
                });
            }
        }
    }
}

fn path_tail(path: &str, width: usize) -> String {
    if super::display_width(path) <= width {
        return path.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut used = 1;
    let mut start = path.len();
    for (index, ch) in path.char_indices().rev() {
        let size = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + size > width {
            break;
        }
        used += size;
        start = index;
    }
    format!("…{}", &path[start..])
}

pub(crate) fn scrollbar(
    buf: &mut Buffer,
    track: Rect,
    total: usize,
    cap: usize,
    scroll: usize,
    thumb_color: Color,
    track_color: Color,
) {
    if total <= cap || track.height == 0 {
        return;
    }
    let len = usize::from(track.height);
    let thumb = (len * cap / total).clamp(1, len);
    let span = total - cap;
    let pos = ((len - thumb) * scroll.min(span))
        .checked_div(span)
        .unwrap_or(0);
    for i in 0..len {
        buf[(track.x, track.y + i as u16)]
            .set_symbol("▕")
            .set_fg(if i >= pos && i < pos + thumb {
                thumb_color
            } else {
                track_color
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rows_reserve_scrollbar_and_keep_path_tail_and_selection() {
        for width in 4..40 {
            let area = Rect::new(0, 0, width, 2);
            let mut buf = Buffer::empty(area);
            draw(
                &mut buf,
                area,
                WorkspaceRow {
                    name: "项目-workspace",
                    branch: Some("feature/long-branch"),
                    path: "~/projects/long-folder/project10",
                    dot: "●",
                    dot_color: Color::Green,
                    nested: true,
                    active: false,
                    selected: true,
                    hovered: false,
                },
                Palette {
                    accent: Color::Yellow,
                    normal: Color::White,
                    muted: Color::DarkGray,
                    path: Color::Gray,
                    branch: Color::Green,
                    active_bg: Color::Blue,
                    selected_bg: Color::Red,
                },
            );
            assert_eq!(buf[(width - 2, 0)].symbol(), " ");
            assert_eq!(buf[(width - 2, 1)].symbol(), " ");
            assert_eq!(buf[(0, 0)].bg, Color::Red);
            if width >= 12 {
                assert_eq!(buf[(width - 3, 1)].symbol(), "0");
            }
        }
    }

    #[test]
    fn path_tail_respects_terminal_cells() {
        for width in 0..20 {
            assert!(crate::ui::display_width(&path_tail("/home/项目/日本語.rs", width)) <= width);
        }
        assert_eq!(path_tail("~/project", 30), "~/project");
    }
}
