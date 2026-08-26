use ratatui::layout::Rect;

/// Geometry shared by mobile dialogs and action sheets. State and validation
/// stay with their existing features; this helper only removes desktop margins.
pub(crate) fn full_screen(screen: Rect) -> Rect {
    screen
}
