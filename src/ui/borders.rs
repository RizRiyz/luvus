//! Pane borders, drawn with ratatui's native `Block` border widget.
//!
//! Each split pane gets a `Block::bordered()` (the standard `│─┌┐` box) painted
//! as an overlay after the pane content. The focused pane's border is brighter
//! (`border_focus` vs `border`); hovering a divider brightens the panes it
//! separates so it reads as draggable.

use super::*;
use ratatui::widgets::{BorderType, Borders};

/// True if the hovered divider `d` runs along one of `rect`'s edges (so the
/// panes it separates should highlight).
fn divider_touches(rect: Rect, d: &crate::layout::Divider) -> bool {
    use crate::layout::Axis;
    let right = rect.x + rect.width.saturating_sub(1);
    let bottom = rect.y + rect.height.saturating_sub(1);
    let near = |line: u16, a: u16, b: u16| {
        (line as i32 - a as i32).abs() <= 1 || (line as i32 - b as i32).abs() <= 1
    };
    match d.axis {
        Axis::Col => near(d.line, rect.x, right) && d.span.0 < bottom && d.span.1 > rect.y,
        Axis::Row => near(d.line, rect.y, bottom) && d.span.0 < right && d.span.1 > rect.x,
    }
}

/// Draw a native ratatui border around every pane. The focused pane (or a pane
/// touched by the hovered divider) uses the brighter `border_focus` colour.
pub(super) fn render_pane_borders(
    f: &mut RenderTarget,
    rects: &[(PaneId, Rect)],
    focus: Option<PaneId>,
    hover: Option<&crate::layout::Divider>,
    t: &Theme,
) {
    if rects.len() < 2 {
        return;
    }
    for (id, rect) in rects {
        if rect.width < 2 || rect.height < 2 {
            continue;
        }
        let focused = focus == Some(*id) || hover.is_some_and(|d| divider_touches(*rect, d));
        let color = if focused { t.border_focus } else { t.border };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .border_style(Style::new().fg(color));
        f.render_widget(block, *rect);
    }
}
