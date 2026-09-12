//! The "what's actually running in this pane?" overlay from its context menu.
//!
//! Agent CLIs elide long tool commands on screen (`Bash(cargo test …)`), and the
//! elided characters are never sent to the terminal, so luvus cannot expand what
//! it was never given. What luvus *does* have is the pane's child pid, so this
//! reads the process tree from the OS and shows every command in full.
//!
//! Drawn last over a dimmed backdrop, like the help overlay; any key or click
//! dismisses it (see `app/input.rs`), `j`/`k` scroll and `r` re-reads.

use super::help::{centered_rect, dim_backdrop, hline};
use super::*;
use crate::app::CmdInspect;
use ratatui::widgets::{Borders, Clear};

pub(super) fn draw(f: &mut RenderTarget, area: Rect, c: &CmdInspect, t: &Theme) {
    dim_backdrop(f, area, t);

    let w = area.width.saturating_sub(6).clamp(40, 100).min(area.width);
    let h = (c.procs.len() as u16 + 6)
        .clamp(9, area.height.saturating_sub(2))
        .min(area.height);
    let modal = centered_rect(area, w, h);
    f.render_widget(Clear, modal);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(t.border_focus).bg(t.surface0))
        .style(Style::new().bg(t.surface0));
    let inner = block.inner(modal);
    f.render_widget(block, modal);
    if inner.height < 3 {
        return;
    }

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" Running now", Style::new().fg(t.text).bold()),
            Span::styled(format!("   pane {}", c.pane.0), Style::new().fg(t.overlay0)),
        ])),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    hline(f, inner.x, inner.y + 1, inner.width, t);

    let body = Rect::new(
        inner.x,
        inner.y + 2,
        inner.width,
        inner.height.saturating_sub(3),
    );
    let cap = body.height as usize;

    if c.procs.is_empty() {
        // No pid, or the platform can't introspect (Windows — docs/16).
        f.render_widget(
            Paragraph::new(Span::styled(
                " nothing running (just the shell)",
                Style::new().fg(t.overlay0),
            )),
            Rect::new(body.x, body.y, body.width, 1),
        );
    } else {
        let max_scroll = c.procs.len().saturating_sub(cap);
        let scroll = c.scroll.min(max_scroll);
        for (i, p) in c.procs.iter().skip(scroll).take(cap).enumerate() {
            let y = body.y + i as u16;
            // The pane's own shell is depth 0; anything deeper is what the agent
            // actually spawned, so highlight those.
            let (marker, fg) = if p.depth == 0 {
                ("shell", t.overlay0)
            } else {
                ("run", t.accent)
            };
            let indent = "  ".repeat((p.depth as usize).min(6));
            let head = format!(" {indent}{marker} ");
            let used = display_width(&head) as u16 + 8;
            let cmd_w = body.width.saturating_sub(used) as usize;
            // Truncate only for *display*; the full string is in `c.procs`.
            let cmd: String = if p.command.chars().count() > cmd_w && cmd_w > 1 {
                let keep: String = p.command.chars().take(cmd_w.saturating_sub(1)).collect();
                format!("{keep}…")
            } else {
                p.command.clone()
            };
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(head, Style::new().fg(fg)),
                    Span::styled(format!("{:>6} ", p.pid), Style::new().fg(t.overlay0)),
                    Span::styled(
                        cmd,
                        Style::new().fg(if p.depth == 0 { t.subtext0 } else { t.text }),
                    ),
                ])),
                Rect::new(body.x, y, body.width, 1),
            );
        }
    }

    // Footer: the pane's folder + the keys.
    let footer_y = inner.bottom().saturating_sub(1);
    let cwd = short_path(&c.cwd, inner.width.saturating_sub(22));
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!(" {cwd}"), Style::new().fg(t.overlay0)),
            Span::styled("   r refresh · any key close", Style::new().fg(t.overlay0)),
        ])),
        Rect::new(inner.x, footer_y, inner.width, 1),
    );
}
