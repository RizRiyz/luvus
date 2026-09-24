//! Commander composer, exact-target syntax, and App-owned delivery.
//! Only input/render integration remains at the existing app and UI boundaries.

mod actions;
mod app;
mod composer;
mod targets;

pub(crate) use actions::SLASH_ACTIONS;
pub(crate) use composer::{line_end, line_start, next_word, previous_word, Commander};
pub(crate) use targets::{
    encode_component, parse_scoped_target, target_lookup, target_spans, unescape_pane_mentions,
    DeliveryPlan, ExactTarget, ScopedTarget, MAX_TARGETS,
};

/// The picker shares Commander's horizontal bounds and uses only the space
/// above it. Both rendering and mouse hit-testing use this geometry.
pub(crate) fn slash_popup_layout(
    strip: ratatui::layout::Rect,
    pane_top: u16,
    count: usize,
) -> Option<(ratatui::layout::Rect, usize)> {
    let available = strip.y.saturating_sub(pane_top);
    if available < 4 || strip.width < 28 {
        return None;
    }
    let visible = count.max(1).min(available.saturating_sub(3) as usize);
    let height = visible as u16 + 3;
    Some((
        ratatui::layout::Rect::new(strip.x, strip.y - height, strip.width, height),
        visible,
    ))
}

pub(crate) fn slash_window_start(selected: usize, total: usize, visible: usize) -> usize {
    selected
        .saturating_sub(visible.saturating_sub(1))
        .min(total.saturating_sub(visible))
}
