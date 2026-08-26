use ratatui::layout::Rect;

pub(super) const HEADER_HEIGHT: u16 = 2;
pub(super) const PRIMARY_BUTTON_WIDTH: u16 = 7;
pub(super) const PRIMARY_BUTTON_HEIGHT: u16 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MobileProfile {
    Desktop,
    Mobile,
}

/// Resolve the presentation from this render viewport only. A zero threshold
/// keeps the existing compatibility behavior of disabling automatic mobile UI.
pub(crate) fn resolve_profile(width: u16, threshold: u16) -> MobileProfile {
    if threshold != 0 && width <= threshold {
        MobileProfile::Mobile
    } else {
        MobileProfile::Desktop
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MobileLayout {
    pub screen: Rect,
    pub header: Rect,
    pub menu_button: Rect,
    pub content: Rect,
    pub notification: Option<Rect>,
}

pub(crate) fn compute_layout(screen: Rect) -> MobileLayout {
    let header_height = HEADER_HEIGHT.min(screen.height);
    let header = Rect::new(screen.x, screen.y, screen.width, header_height);
    let button_width = PRIMARY_BUTTON_WIDTH.min(screen.width);
    let menu_button = Rect::new(
        screen.right().saturating_sub(button_width),
        screen.y,
        button_width,
        PRIMARY_BUTTON_HEIGHT.min(header_height),
    );
    let content = Rect::new(
        screen.x,
        screen.y.saturating_add(header_height),
        screen.width,
        screen.height.saturating_sub(header_height),
    );
    MobileLayout {
        screen,
        header,
        menu_button,
        content,
        notification: None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct MobileNavigatorLayout {
    pub header: Rect,
    pub close_button: Rect,
    pub scopes: Rect,
    pub query: Rect,
    pub viewport: Rect,
    pub scrollbar: Option<Rect>,
}

pub(super) fn navigator_layout(screen: Rect) -> MobileNavigatorLayout {
    let header_height = PRIMARY_BUTTON_HEIGHT.min(screen.height);
    let close_width = PRIMARY_BUTTON_WIDTH.min(screen.width);
    let header = Rect::new(screen.x, screen.y, screen.width, header_height);
    let close_button = Rect::new(
        screen.right().saturating_sub(close_width),
        screen.y,
        close_width,
        header_height,
    );
    let scopes_y = screen.y.saturating_add(header_height);
    let scopes_h = u16::from(scopes_y < screen.bottom());
    let query_y = scopes_y.saturating_add(scopes_h);
    let query_h = u16::from(query_y < screen.bottom());
    let viewport_y = query_y.saturating_add(query_h);
    let viewport = Rect::new(
        screen.x,
        viewport_y,
        screen.width,
        screen.bottom().saturating_sub(viewport_y),
    );
    MobileNavigatorLayout {
        header,
        close_button,
        scopes: Rect::new(screen.x, scopes_y, screen.width, scopes_h),
        query: Rect::new(screen.x, query_y, screen.width, query_h),
        viewport,
        scrollbar: (viewport.width > 0 && viewport.height > 0).then(|| {
            Rect::new(
                viewport.right().saturating_sub(1),
                viewport.y,
                1,
                viewport.height,
            )
        }),
    }
}
