//! Rendering. compute (resize PTYs) then a pure draw pass: chrome (sidebar,
//! tab bar, status) plus the tiled panes. See docs/06-ui-rendering.md.

pub mod theme;

use std::path::Path;

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Widget};
use ratatui::Frame;

use crate::app::{App, DockKind, Mode, Side};
use crate::ids::PaneId;
use crate::ui::theme::{State, Theme};

/// A draw surface mirroring the slice of `ratatui::Frame` the UI actually uses, but
/// over a `Buffer` we own. The server renders straight into its frame buffer through
/// this and runs a single `diff_buffer`, skipping `Terminal`'s redundant internal
/// reset+diff+flush (~28% of the per-frame cost — see `bench_render_hotpath`). The
/// `--local` path and tests keep calling `render(&mut Frame, …)`, which wraps the
/// terminal's own buffer in one of these.
pub struct RenderTarget<'a> {
    buf: &'a mut Buffer,
    area: Rect,
    cursor: Option<(u16, u16)>,
    cursor_visible: bool,
    animation_mask: AnimationMask,
}

/// Allocation-free record of animated surfaces that were actually drawn into a
/// client projection. The server uses this instead of global agent state, which
/// avoids repainting when every working indicator is clipped or hidden.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AnimationMask(u8);

impl AnimationMask {
    const WORKING_SPINNER: u8 = 1 << 0;

    pub fn has_working_spinner(self) -> bool {
        self.0 & Self::WORKING_SPINNER != 0
    }
}

impl<'a> RenderTarget<'a> {
    /// Wrap a buffer we own (the server's frame buffer) as a draw surface.
    pub fn new(buf: &'a mut Buffer, area: Rect) -> Self {
        RenderTarget {
            buf,
            area,
            cursor: None,
            cursor_visible: false,
            animation_mask: AnimationMask::default(),
        }
    }
    pub fn area(&self) -> Rect {
        self.area
    }
    pub fn buffer_mut(&mut self) -> &mut Buffer {
        self.buf
    }
    pub fn render_widget<W: Widget>(&mut self, widget: W, area: Rect) {
        widget.render(area, self.buf);
    }
    pub fn set_cursor_position<P: Into<Position>>(&mut self, position: P) {
        let p = position.into();
        self.cursor = Some((p.x, p.y));
        self.cursor_visible = true;
    }
    pub fn set_cursor_anchor(&mut self, x: u16, y: u16, visible: bool) {
        self.cursor = Some((x, y));
        self.cursor_visible = visible;
    }
    pub fn cursor(&self) -> Option<(u16, u16)> {
        self.cursor
    }
    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    /// Mark that this projection contains a live working-state spinner.
    pub fn mark_working_animation(&mut self) {
        self.animation_mask.0 |= AnimationMask::WORKING_SPINNER;
    }

    pub fn animation_mask(&self) -> AnimationMask {
        self.animation_mask
    }
}

mod board;
mod borders;
pub(crate) mod changelog;
mod cmdinfo;
mod diff;
mod files;
mod git;
mod help;
mod menu;
mod mission;
mod mobile;
mod panes;
mod picker;
mod search;
mod settings;
mod sidebar;
mod status;
pub(crate) mod switcher;
mod tabbar;

/// Frame-based entry (used by `--local` and tests): wrap the terminal's buffer in a
/// `RenderTarget`, render, then copy the resulting cursor back onto the frame.
pub fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let (cursor, visible) = {
        let mut target = RenderTarget {
            buf: f.buffer_mut(),
            area,
            cursor: None,
            cursor_visible: false,
            animation_mask: AnimationMask::default(),
        };
        render_into(&mut target, app);
        (target.cursor, target.cursor_visible)
    };
    if let Some(p) = cursor {
        if visible {
            f.set_cursor_position(p);
        }
    }
}

/// The actual UI render, over a buffer we own (`RenderTarget`). The server calls
/// this directly with its frame buffer; `render` above adapts a `Frame` to it.
pub fn render_into(f: &mut RenderTarget, app: &mut App) {
    render_into_mode(f, app, true);
}

/// Render a secondary client's viewport without letting that projection become
/// the interactive view or resize the shared PTYs.
///
/// Luvus deliberately keeps one server-owned application state. Multi-client
/// displays may have different dimensions, though, so the server renders each
/// secondary viewport independently. The renderer records hit-test geometry and
/// clamps a handful of scroll offsets as part of an ordinary interactive draw;
/// preserve those values here so a passive projection cannot move the active
/// client's cursor, scroll position, compact mode, or click targets.
pub fn render_projection(f: &mut RenderTarget, app: &mut App) {
    let compact = app.compact;
    let last_main_area = app.last_main_area;
    let last_pane_area = app.last_pane_area;
    let left_seam = app.left_seam;
    let right_seam = app.right_seam;
    let last_cursor = app.last_cursor;
    let workspaces_scroll = app.workspaces_scroll;
    let agents_scroll = app.agents_scroll;
    let last_active_ws_shown = app.last_active_ws_shown;
    let switcher_scroll = app.switcher_scroll;
    let orch_scroll = app.orch_scroll;
    let orch_detail_scroll = app.orch_detail_scroll;
    let orch_area = app.orch_area;
    let mission_scroll = app.mission_scroll;
    let mission_area = app.mission_area;
    let mission_refresh_rect = app.mission_refresh_rect;
    let changelog_scroll = app.changelog_scroll;
    let file_tree_scroll = app.file_tree.scroll;
    // Popup scroll offsets and geometry are recorded by an ordinary draw, and a
    // projection renders at its own size — so keep them out of its way.
    let menu_scroll = std::mem::take(&mut app.menu_scroll);

    // Geometry collections are write-only outputs of a render. Move the active
    // client's values aside instead of cloning them on every secondary frame.
    let pane_rects = std::mem::take(&mut app.pane_rects);
    let pane_content_rects = std::mem::take(&mut app.pane_content_rects);
    let pane_title_rects = std::mem::take(&mut app.pane_title_rects);
    let tab_rects = std::mem::take(&mut app.tab_rects);
    let tab_close_rects = std::mem::take(&mut app.tab_close_rects);
    let ws_rects = std::mem::take(&mut app.ws_rects);
    let git_section_rects = std::mem::take(&mut app.git_section_rects);
    let agents_filter_rects = std::mem::take(&mut app.agents_filter_rects);
    let agent_rects = std::mem::take(&mut app.agent_rects);
    let session_rects = std::mem::take(&mut app.session_rects);
    let file_tree_rects = std::mem::take(&mut app.file_tree_rects);
    let files_mode_rects = std::mem::take(&mut app.files_mode_rects);
    let diff_row_rects = std::mem::take(&mut app.diff_row_rects);
    let diff_source_rects = std::mem::take(&mut app.diff_source_rects);
    let diff_note_rects = std::mem::take(&mut app.diff_note_rects);
    let module_dock_rects = std::mem::take(&mut app.module_dock_rects);
    let picker_rects = std::mem::take(&mut app.picker_rects);
    let settings_tab_rects = std::mem::take(&mut app.settings_tab_rects);
    let settings_ctl_rects = std::mem::take(&mut app.settings_ctl_rects);
    let settings_theme_remove_rects = std::mem::take(&mut app.settings_theme_remove_rects);
    let settings_arrow_rects = std::mem::take(&mut app.settings_arrow_rects);
    let changelog_link_rects = std::mem::take(&mut app.changelog_link_rects);
    let changelog_copy_rects = std::mem::take(&mut app.changelog_copy_rects);
    let switcher_rects = std::mem::take(&mut app.switcher_rects);
    let switcher_scope_rects = std::mem::take(&mut app.switcher_scope_rects);
    let mission_rows = std::mem::take(&mut app.mission_rows);
    let mission_scope_rects = std::mem::take(&mut app.mission_scope_rects);
    let bar_hits = std::mem::take(&mut app.bar.hits);
    let bar_overflow_hits = std::mem::take(&mut app.bar.overflow_hits);
    let bar_overflow = app.bar.overflow.clone();
    let search_rects = app
        .search
        .as_mut()
        .map(|search| std::mem::take(&mut search.rects));
    let ws_menu_items = app
        .ws_menu
        .as_mut()
        .map(|menu| std::mem::take(&mut menu.items));
    let tab_menu_state = app.tab_menu.as_mut().map(|menu| {
        (
            std::mem::take(&mut menu.items),
            std::mem::take(&mut menu.swap_rects),
            menu.swap_open,
        )
    });
    let pane_menu_state = app.pane_menu.as_mut().map(|menu| {
        (
            std::mem::take(&mut menu.items),
            std::mem::take(&mut menu.tab_rects),
            menu.move_open,
        )
    });
    let agent_menu_items = app
        .agent_menu
        .as_mut()
        .map(|menu| std::mem::take(&mut menu.items));
    let file_menu_items = app
        .file_menu
        .as_mut()
        .map(|menu| std::mem::take(&mut menu.items));
    let diff_menu_items = app
        .diff_menu
        .as_mut()
        .map(|menu| std::mem::take(&mut menu.items));
    let dock_menu_rects = app
        .dock_menu
        .as_mut()
        .map(|menu| std::mem::take(&mut menu.rects));

    let settings_icon_rect = app.settings_icon_rect;
    let sidebar_toggle_rect = app.sidebar_toggle_rect;
    let right_sidebar_toggle_rect = app.right_sidebar_toggle_rect;
    let version_rect = app.version_rect;
    let files_area = app.files_area;
    let workspaces_area = app.workspaces_area;
    let agents_area = app.agents_area;
    let pane_close_rect = app.pane_close_rect;
    let pane_zoom_rect = app.pane_zoom_rect;
    let tab_prev_rect = app.tab_prev_rect;
    let tab_next_rect = app.tab_next_rect;
    let new_ws_rect = app.new_ws_rect;
    let switcher_button_rect = app.switcher_button_rect;
    let mobile_pane_prev_rect = app.mobile_pane_prev_rect;
    let mobile_pane_next_rect = app.mobile_pane_next_rect;
    let switcher_close_rect = app.switcher_close_rect;
    let settings_modal_rect = app.settings_modal_rect;
    let settings_close_rect = app.settings_close_rect;
    let changelog_modal_rect = app.changelog_modal_rect;
    let changelog_close_rect = app.changelog_close_rect;
    let changelog_check_rect = app.changelog_check_rect;
    let modal_commit_rect = app.modal_commit_rect;
    let modal_cancel_rect = app.modal_cancel_rect;

    // Git rendering also stores its list viewport and clamps detail scroll.
    let git_view = app.active_git_mut().map(|git| {
        (
            git.id,
            git.scroll,
            git.list_area,
            git.contributors_more_rect,
        )
    });

    render_into_mode(f, app, false);

    app.compact = compact;
    app.last_main_area = last_main_area;
    app.last_pane_area = last_pane_area;
    app.left_seam = left_seam;
    app.right_seam = right_seam;
    app.last_cursor = last_cursor;
    app.workspaces_scroll = workspaces_scroll;
    app.agents_scroll = agents_scroll;
    app.last_active_ws_shown = last_active_ws_shown;
    app.switcher_scroll = switcher_scroll;
    app.orch_scroll = orch_scroll;
    app.orch_detail_scroll = orch_detail_scroll;
    app.orch_area = orch_area;
    app.mission_scroll = mission_scroll;
    app.mission_area = mission_area;
    app.mission_refresh_rect = mission_refresh_rect;
    app.changelog_scroll = changelog_scroll;
    app.file_tree.scroll = file_tree_scroll;
    app.menu_scroll = menu_scroll;
    app.pane_rects = pane_rects;
    app.pane_content_rects = pane_content_rects;
    app.pane_title_rects = pane_title_rects;
    app.tab_rects = tab_rects;
    app.tab_close_rects = tab_close_rects;
    app.ws_rects = ws_rects;
    app.git_section_rects = git_section_rects;
    app.agents_filter_rects = agents_filter_rects;
    app.agent_rects = agent_rects;
    app.session_rects = session_rects;
    app.file_tree_rects = file_tree_rects;
    app.files_mode_rects = files_mode_rects;
    app.diff_row_rects = diff_row_rects;
    app.diff_source_rects = diff_source_rects;
    app.diff_note_rects = diff_note_rects;
    app.module_dock_rects = module_dock_rects;
    app.picker_rects = picker_rects;
    app.settings_tab_rects = settings_tab_rects;
    app.settings_ctl_rects = settings_ctl_rects;
    app.settings_theme_remove_rects = settings_theme_remove_rects;
    app.settings_arrow_rects = settings_arrow_rects;
    app.changelog_link_rects = changelog_link_rects;
    app.changelog_copy_rects = changelog_copy_rects;
    app.switcher_rects = switcher_rects;
    app.switcher_scope_rects = switcher_scope_rects;
    app.mission_rows = mission_rows;
    app.mission_scope_rects = mission_scope_rects;
    app.bar.hits = bar_hits;
    app.bar.overflow_hits = bar_overflow_hits;
    app.bar.overflow = bar_overflow;
    if let (Some(rects), Some(search)) = (search_rects, app.search.as_mut()) {
        search.rects = rects;
    }
    if let (Some(items), Some(menu)) = (ws_menu_items, app.ws_menu.as_mut()) {
        menu.items = items;
    }
    if let (Some((items, swap_rects, swap_open)), Some(menu)) =
        (tab_menu_state, app.tab_menu.as_mut())
    {
        menu.items = items;
        menu.swap_rects = swap_rects;
        menu.swap_open = swap_open;
    }
    if let (Some((items, tab_rects, move_open)), Some(menu)) =
        (pane_menu_state, app.pane_menu.as_mut())
    {
        menu.items = items;
        menu.tab_rects = tab_rects;
        menu.move_open = move_open;
    }
    if let (Some(items), Some(menu)) = (agent_menu_items, app.agent_menu.as_mut()) {
        menu.items = items;
    }
    if let (Some(items), Some(menu)) = (file_menu_items, app.file_menu.as_mut()) {
        menu.items = items;
    }
    if let (Some(items), Some(menu)) = (diff_menu_items, app.diff_menu.as_mut()) {
        menu.items = items;
    }
    if let (Some(rects), Some(menu)) = (dock_menu_rects, app.dock_menu.as_mut()) {
        menu.rects = rects;
    }
    app.settings_icon_rect = settings_icon_rect;
    app.sidebar_toggle_rect = sidebar_toggle_rect;
    app.right_sidebar_toggle_rect = right_sidebar_toggle_rect;
    app.version_rect = version_rect;
    app.files_area = files_area;
    app.workspaces_area = workspaces_area;
    app.agents_area = agents_area;
    app.pane_close_rect = pane_close_rect;
    app.pane_zoom_rect = pane_zoom_rect;
    app.tab_prev_rect = tab_prev_rect;
    app.tab_next_rect = tab_next_rect;
    app.new_ws_rect = new_ws_rect;
    app.switcher_button_rect = switcher_button_rect;
    app.mobile_pane_prev_rect = mobile_pane_prev_rect;
    app.mobile_pane_next_rect = mobile_pane_next_rect;
    app.switcher_close_rect = switcher_close_rect;
    app.settings_modal_rect = settings_modal_rect;
    app.settings_close_rect = settings_close_rect;
    app.changelog_modal_rect = changelog_modal_rect;
    app.changelog_close_rect = changelog_close_rect;
    app.changelog_check_rect = changelog_check_rect;
    app.modal_commit_rect = modal_commit_rect;
    app.modal_cancel_rect = modal_cancel_rect;
    if let Some((id, scroll, list_area, contributors_more_rect)) = git_view {
        if let Some(git) = app.active_git_mut().filter(|git| git.id == id) {
            git.scroll = scroll;
            git.list_area = list_area;
            git.contributors_more_rect = contributors_more_rect;
        }
    }
}

fn render_into_mode(f: &mut RenderTarget, app: &mut App, resize_panes: bool) {
    let t = app.theme.clone();
    // The active i18n catalog (Copy `&'static`), passed to draw fns that don't
    // get the whole `App` (picker, git tab) so all chrome is localized (docs/21).
    let cat = app.catalog;
    let area = f.area();
    f.render_widget(Block::new().style(Style::new().bg(t.mantle)), area);
    app.bar.hits.clear();
    app.bar.overflow_hits.clear();
    app.menu_scroll.begin_frame();
    app.mobile_pane_prev_rect = None;
    app.mobile_pane_next_rect = None;

    // An absurdly small window can't hold the chrome — say so instead of
    // drawing degraded fragments. (Every draw fn is underflow-safe regardless;
    // this is purely the friendlier message.)
    if area.width < 24 || area.height < 6 {
        if area.height > 0 {
            let msg = "◱ enlarge terminal";
            let y = area.y + area.height / 2;
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(msg, Style::new().fg(t.overlay1)))),
                Rect::new(area.x, y, area.width, 1),
            );
        }
        return;
    }

    // No nodes at all (docs/43 §3.3): the last one was closed, so the session is
    // over and every client has been told to detach. A client can still be
    // attached for the frame or two before it goes (or attach fresh via
    // `luvus attach` / `--remote` before opening a folder), and every draw fn
    // below assumes an active node — `app.ws()` indexes `workspaces[active_ws]`.
    // One guard here covers the whole tree rather than each call site.
    if app.workspaces.is_empty() {
        let msg = "no folders open — run `luvus` in a folder";
        let y = area.y + area.height / 2;
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(msg, Style::new().fg(t.overlay1))))
                .alignment(ratatui::layout::Alignment::Center),
            Rect::new(area.x, y, area.width, 1),
        );
        return;
    }

    // Automatic mobile presentation is derived from this client's viewport.
    // `app.compact` remains a compatibility flag for existing compact renderers,
    // but it is never persisted and projection rendering restores it afterward.
    app.compact = matches!(
        mobile::resolve_profile(area.width, app.config.layout.mobile_width),
        mobile::MobileProfile::Mobile
    );
    app.refresh_core_bar_widgets();
    let status_h = if app.compact { 0 } else { 1 };
    let [main, status] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(status_h)]).areas(area);
    // Stored so an in-flight sidebar-edge drag can map a cursor column to a width
    // off the correct edge (docs/29).
    app.last_main_area = main;

    // Two sidebars flank the content (docs/29). Each is shown only if visible,
    // non-empty, and it (with the other) leaves the panes at least 24 columns —
    // the right yields space first, then the left. A width that would fall below
    // the minimum drops the sidebar entirely (matching the original behavior).
    let min = crate::app::SIDEBAR_WIDTH_MIN;
    let fit = |w: u16, budget: u16| -> u16 {
        let w = w.min(budget.saturating_sub(24));
        if w >= min {
            w
        } else {
            0
        }
    };
    let lw = if app.compact || !app.sidebars.left.shown() {
        0
    } else {
        fit(app.sidebars.left.width, main.width)
    };
    let rw = if app.compact || !app.sidebars.right.shown() {
        0
    } else {
        fit(app.sidebars.right.width, main.width.saturating_sub(lw))
    };
    let [left_area, content, right_area] = Layout::horizontal([
        Constraint::Length(lw),
        Constraint::Min(0),
        Constraint::Length(rw),
    ])
    .areas(main);
    let sidebar_left = (lw > 0).then_some(left_area);
    let sidebar_right = (rw > 0).then_some(right_area);
    // The draggable edge seam of each shown sidebar — the `│` column drawn by
    // `draw_sidebar` (left sidebar's right edge, right sidebar's left edge).
    // Recomputed every frame, so a hidden sidebar leaves `None` and its drag can
    // never fire (docs/29).
    app.left_seam = sidebar_left.map(|a| Rect::new(a.right().saturating_sub(1), a.y, 1, a.height));
    app.right_seam = sidebar_right.map(|a| Rect::new(a.x, a.y, 1, a.height));

    let mobile_layout = app.compact.then(|| mobile::compute_layout(content));
    let (tabbar, pane_area) = if let Some(layout) = mobile_layout {
        (layout.header, layout.content)
    } else {
        let [tabbar, pane_area] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(content);
        (tabbar, pane_area)
    };

    app.last_pane_area = pane_area;

    let focus = app.layout().focus;
    // Compact mode shows a single full-screen pane (like zoom) — tiling is
    // useless on a phone; the switcher handles navigation.
    let rects: Vec<(PaneId, Rect)> = if app.zoomed || app.compact {
        vec![(focus, pane_area)]
    } else {
        app.layout()
            .panes(pane_area)
            .into_iter()
            .map(|p| (p.id, p.rect))
            .collect()
    };
    // Only frame panes when the tab is split; a lone pane needs no border.
    let bordered = rects.len() > 1;
    if resize_panes {
        for (id, rect) in &rects {
            let Some(content) = pane_content(*rect, bordered, app.compact) else {
                continue;
            };
            let resized = app
                .panes
                .get_mut(id)
                .map(|p| p.resize(content.width, content.height))
                .unwrap_or(false);
            // A real resize (e.g. switching to a tab whose panes have a different
            // geometry) repaints the agent; note it so detection freezes briefly
            // and a reflowed spinner can't flip the pane to "working" (docs/07).
            if resized {
                if let Some(s) = app.status.get_mut(id) {
                    s.last_resize = Some(std::time::Instant::now());
                    s.force_detect = true;
                }
            }
        }
    }

    // Clear every dock/sidebar hit-geometry up front, then let each drawn dock
    // set its own. A dock mounted nowhere (or a hidden sidebar) leaves its rects
    // zeroed, so nothing fires from under a widened pane area (docs/29).
    app.settings_icon_rect = None;
    app.sidebar_toggle_rect = None;
    app.right_sidebar_toggle_rect = None;
    app.version_rect = None;
    app.workspaces_area = Rect::ZERO;
    app.agents_area = Rect::ZERO;
    app.agents_filter_rects.clear();
    app.module_dock_rects.clear();
    // The FILES dock's geometry must be zeroed here too, or its row rects go stale
    // when it isn't drawn this frame (its sidebar hidden, or the dock moved/off as
    // the user rearranges docks). A left click checks `file_tree_rects` before
    // `ws_rects`, so a stale file rect left over a relocated WORKSPACES row opens a
    // file instead of switching workspace (docs/29 + docs/38).
    app.files_area = Rect::ZERO;
    app.file_tree_rects.clear();
    app.files_mode_rects.clear();
    app.diff_row_rects.clear();
    app.diff_source_rects.clear();
    app.diff_note_rects.clear();
    let mut ws_rects = Vec::new();
    let mut agent_rects = Vec::new();
    let mut session_rects = Vec::new();
    let mut new_ws_rect = None;
    for (opt, side) in [(sidebar_left, Side::Left), (sidebar_right, Side::Right)] {
        if let Some(s) = opt {
            let (w, a, se, n) = sidebar::draw_sidebar(f, side, s, app, &t);
            ws_rects.extend(w);
            agent_rects.extend(a);
            session_rects.extend(se);
            new_ws_rect = new_ws_rect.or(n);
        }
    }
    let (tab_rects, tab_close_rects, tab_prev, tab_next) = if let Some(layout) = mobile_layout {
        app.sidebar_toggle_rect = None;
        app.right_sidebar_toggle_rect = None;
        mobile::render_header(f, layout, app, &t);
        (Vec::new(), Vec::new(), None, None)
    } else {
        tabbar::draw_tabbar(f, tabbar, app, &t)
    };
    // Behind the panes, use the (dark) pane background.
    f.render_widget(Block::new().style(Style::new().bg(t.mantle)), pane_area);
    // The focused pane's ✕ close and ⤢ zoom buttons, for mouse hit-testing.
    let focused_rect = bordered
        .then(|| rects.iter().find(|(id, _)| *id == focus).map(|(_, r)| *r))
        .flatten();
    app.pane_close_rect = focused_rect.and_then(|r| pane_close_rect(r, bordered));
    // Zoom button: the split-title ⤢ when bordered, else the lone-header ⤡ that
    // restores a *zoomed* single pane (so a phone can un-zoom).
    app.pane_zoom_rect = if bordered {
        focused_rect.and_then(|r| pane_zoom_rect(r, bordered))
    } else if app.zoomed {
        rects
            .iter()
            .find(|(id, _)| *id == focus)
            .map(|(_, r)| lone_zoom_rect(*r))
            .filter(|r| r.width >= 3)
    } else {
        None
    };
    // A git tab / orchestration board fills the pane area with a dashboard
    // instead of terminals.
    let mut git_section_rects = Vec::new();
    let mut title_rects: Vec<(PaneId, Rect)> = Vec::new();
    // Captured before the `active_git_mut` borrow below; the dashboards drop
    // their keyboard-hint footer on a phone (docs/18) and give it to the list.
    let compact = app.compact;
    let cursor = if app.active_is_orch() {
        app.orch_area = pane_area;
        // Each task's live worker state (detection) rides along on its row.
        let live: Vec<Option<board::RowLive>> = app
            .orch
            .tasks
            .iter()
            .map(|task| {
                task.assignee
                    .map(crate::ids::PaneId)
                    .and_then(|pid| app.status.get(&pid))
                    .map(|st| board::RowLive {
                        agent: st.agent.clone(),
                        state: st.state,
                    })
            })
            .collect();
        app.orch_scroll = board::render(
            f,
            pane_area,
            &app.orch,
            &live,
            app.orch_scroll,
            app.orch_cursor,
            compact,
            cat,
            &t,
        );
        None
    } else if app.active_is_mission() {
        // Mission Control (docs/54): rows are precomputed from `App` first (so the
        // render borrows nothing mutable), stashed for keyboard activation, then
        // drawn; the scroll offset is written back.
        app.mission_area = pane_area;
        let rows = app.build_mission_rows();
        let rendered = mission::render(
            f,
            pane_area,
            &rows,
            app.mission_scroll,
            app.mission_cursor,
            app.mission_scope,
            app.mission_usage_refreshing(),
            app.mission_burn,
            app.config.mission_budget,
            compact,
            cat,
            &t,
        );
        app.mission_scroll = rendered.scroll;
        app.mission_scope_rects = rendered.scope_rects;
        app.mission_refresh_rect = rendered.refresh_rect;
        app.mission_rows = rows;
        None
    } else if let Some(g) = app.active_git_mut() {
        git_section_rects = git::render(f, pane_area, g, compact, cat, &t);
        None
    } else {
        let cursor = panes::draw_panes(f, &rects, bordered, app, &t);
        // Draw all pane borders in one overlay pass (manual cell-by-cell), then
        // the dot+path+close titles ON each top border row.
        if bordered {
            borders::render_pane_borders(f, &rects, focus, app.hover_divider.as_ref(), &t);
            if app.config.layout.show_titles {
                title_rects = panes::draw_pane_titles(f, &rects, focus, app, &t);
            }
        }
        cursor
    };
    app.git_section_rects = git_section_rects;
    app.pane_title_rects = title_rects;
    // Per-pane content rects so mouse drags map to grid cells for text selection
    // (a git tab has no selectable terminal panes).
    app.pane_content_rects =
        if app.active_is_git() || app.active_is_orch() || app.active_is_mission() {
            Vec::new()
        } else {
            rects
                .iter()
                .filter_map(|(id, r)| pane_content(*r, bordered, app.compact).map(|c| (*id, c)))
                .collect()
        };
    status::draw_status(f, status, app, &t);

    // Read-only overflow is attachment-local geometry over server-owned bar
    // content. Draw it above chrome and panes, below modal workflows.
    crate::bar::render::draw_overflow(f, area, &mut app.bar, &t);

    // The Settings modal draws last, on top of everything, and owns the cursor.
    let settings_hits = app
        .settings
        .is_some()
        .then(|| settings::draw_settings(f, area, app, &t));
    if let Some(h) = &settings_hits {
        app.settings_modal_rect = Some(h.modal);
        app.settings_close_rect = Some(h.close);
        app.settings_tab_rects = h.tabs.clone();
        app.settings_ctl_rects = h.ctls.clone();
        app.settings_theme_remove_rects = h.theme_remove.clone();
        app.settings_arrow_rects = h.arrows.clone();
        if let Some(settings) = app.settings.as_mut() {
            settings.layout_scroll = h.layout_scroll;
        }
    } else {
        app.settings_modal_rect = None;
        app.settings_close_rect = None;
        app.settings_tab_rects.clear();
        app.settings_ctl_rects.clear();
        app.settings_theme_remove_rects.clear();
        app.settings_arrow_rects.clear();
    }
    // A module-setting prompt sits above the modal it was opened from.
    settings::draw_module_setting_prompt(f, area, app, &t);

    // The folder picker also draws last (over everything) when open.
    let picker_open = app.picker.is_some();
    let mut picker_rects = Vec::new();
    if let Some(p) = &app.picker {
        picker_rects = picker::draw_picker(f, area, p, app.compact, cat, &t);
    }
    app.picker_rects = picker_rects;

    // The keyboard cheat-sheet overlay draws on top of everything.
    if app.help_open {
        help::draw_help(f, area, app, &t);
    }
    // The changelog modal (click the version number) draws on top too. Clear its
    // close-button geometry when shut so a stale rect can't fire.
    if app.changelog_open {
        changelog::draw_changelog(f, area, app, &t);
    } else {
        app.changelog_modal_rect = None;
        app.changelog_close_rect = None;
        app.changelog_check_rect = None;
        app.changelog_link_rects.clear();
        app.changelog_copy_rects.clear();
    }
    // The running-command overlay (click a pane title) draws above that.
    if let Some(c) = app.cmd_inspect.as_ref() {
        cmdinfo::draw(f, area, c, &t);
    }
    // Text-input modals: each returns the rects of its clickable ⏎/esc footer
    // hints (or `None` while an error occupies the line), stashed so the mouse
    // layer can act on them. Only one is ever open; clear first so a closed modal
    // leaves nothing behind.
    let hover = app.hover;
    app.modal_commit_rect = None;
    app.modal_cancel_rect = None;
    // The new-worktree branch prompt (docs/18 WT).
    if let Some(buf) = app.worktree_prompt.clone() {
        let err = app.worktree_error.clone();
        let (c, x) = picker::draw_worktree_prompt(f, area, &buf, err.as_deref(), hover, cat, &t);
        app.modal_commit_rect = c;
        app.modal_cancel_rect = x;
    }
    // The tab-rename modal (docs/28).
    if let Some(buf) = app.tab_rename.as_ref().map(|r| r.buffer.clone()) {
        let (c, x) = picker::draw_tab_rename(f, area, &buf, hover, cat, &t);
        app.modal_commit_rect = c;
        app.modal_cancel_rect = x;
    }
    // The workspace-rename modal, then the right-click context menu (on top).
    if let Some(buf) = app.ws_rename.as_ref().map(|r| r.buffer.clone()) {
        let (c, x) = picker::draw_ws_rename(f, area, &buf, hover, cat, &t);
        app.modal_commit_rect = c;
        app.modal_cancel_rect = x;
    }
    // The pane-rename modal (same look), from the pane / AGENTS right-click menu.
    if let Some(buf) = app.pane_rename.as_ref().map(|r| r.buffer.clone()) {
        let (c, x) = picker::draw_pane_rename(f, area, &buf, hover, cat, &t);
        app.modal_commit_rect = c;
        app.modal_cancel_rect = x;
    }
    if app.tab_menu.is_some() {
        menu::draw_tab_menu(f, area, app, cat, &t);
    }
    if app.pane_menu.is_some() {
        menu::draw_pane_menu(f, area, app, cat, &t);
    }
    if app.agent_menu.is_some() {
        menu::draw_agent_menu(f, area, app, cat, &t);
    }
    if app.ws_menu.is_some() {
        menu::draw_ws_menu(f, area, app, cat, &t);
    }
    // The FILES-dock context menu + its create/rename/delete modals (docs/38).
    if app.file_menu.is_some() {
        menu::draw_file_menu(f, area, app, &t);
    }
    if app.diff_menu.is_some() {
        menu::draw_diff_menu(f, area, app, &t);
    }
    // A module dock row's own context menu (docs/52).
    if app.dock_menu.is_some() {
        menu::draw_dock_menu(f, area, app, &t);
    }
    if let Some(p) = &app.file_prompt {
        let (title, buf, err) = (
            files::file_prompt_title(p),
            p.buffer.clone(),
            p.error.clone(),
        );
        let (c, x) =
            picker::draw_rename_titled(f, area, title, &buf, err.as_deref(), hover, cat, &t);
        app.modal_commit_rect = c;
        app.modal_cancel_rect = x;
    }
    if let Some(path) = &app.file_delete {
        let (c, x) = files::draw_delete_confirm(f, area, path, None, hover, &t);
        app.modal_commit_rect = c;
        app.modal_cancel_rect = x;
    }
    // Worktree-delete confirm (docs/18 WT): reuses the delete modal, worded for a
    // worktree since it also removes the git worktree, not just a folder.
    let worktree_path = app
        .worktree_delete
        .as_deref()
        .and_then(|id| app.workspaces.iter().find(|workspace| workspace.id == id))
        .map(|w| w.cwd.clone());
    if app.worktree_delete.is_some() && worktree_path.is_none() {
        app.worktree_delete = None;
    }
    if let Some(path) = worktree_path {
        let (c, x) = files::draw_delete_confirm(
            f,
            area,
            &path,
            Some("Delete worktree and its files?"),
            hover,
            &t,
        );
        app.modal_commit_rect = c;
        app.modal_cancel_rect = x;
    }
    // The board's new-task form (docs/22 ORCH-7).
    if let Some(form) = &app.orch_form {
        board::draw_form(f, area, form, cat, &t);
    }
    // The board's start-worker picker and task detail overlay.
    if let Some(start) = &app.orch_start {
        board::draw_start(f, area, start, cat, &t);
    }
    if let Some(id) = &app.orch_detail {
        let clamped = app
            .orch
            .tasks
            .iter()
            .find(|task| &task.id == id)
            .map(|task| board::draw_detail(f, area, task, app.orch_detail_scroll, cat, &t));
        if let Some(s) = clamped {
            app.orch_detail_scroll = s;
        }
    }
    // Mission Control's row-detail overlay and inline answer input (docs/54).
    if app.active_is_mission() {
        if let Some(idx) = app.mission_detail {
            if let Some(row) = app.mission_rows.get(idx) {
                mission::draw_detail(f, area, row, cat, &t);
            }
        }
        if let Some(text) = &app.mission_answer {
            mission::draw_answer(f, area, text, cat, &t);
        }
    }
    // The touch switcher overlay (docs/18), above the chrome but below a toast.
    if app.switcher {
        if app.compact {
            mobile::render_navigator(f, area, app, &t);
        } else {
            app.switcher_close_rect = None;
            switcher::draw_switcher(f, area, app, &t);
        }
    } else {
        app.switcher_rects.clear();
        app.switcher_scope_rects.clear();
        app.switcher_close_rect = None;
    }
    // The global scrollback-search overlay (docs/63), above the chrome.
    if app.search.is_some() {
        search::draw_search(f, area, app, &t);
    }
    // A transient toast (e.g. "Copied") flashes on top of everything.
    if let Some((text, _)) = &app.toast {
        draw_toast(f, area, text, &t);
    }

    let cursor = if settings_hits.is_some()
        || picker_open
        || app.bar.overflow.is_some()
        || app.help_open
        || app.worktree_prompt.is_some()
        || app.tab_rename.is_some()
        || app.tab_menu.is_some()
        || app.ws_rename.is_some()
        || app.pane_rename.is_some()
        || app.ws_menu.is_some()
        || app.pane_menu.is_some()
        || app.agent_menu.is_some()
        || app.file_menu.is_some()
        || app.diff_menu.is_some()
        || app.dock_menu.is_some()
        || app.file_prompt.is_some()
        || app.file_delete.is_some()
        || app.worktree_delete.is_some()
        || app.switcher
        || app.orch_form.is_some()
        || app.orch_start.is_some()
        || app.orch_detail.is_some()
    {
        None
    } else {
        cursor
    };
    if let Some((x, y, visible)) = cursor {
        f.set_cursor_anchor(x, y, visible);
    }
    app.last_cursor = cursor.map(|(x, y, _)| (x, y));
    app.pane_rects = rects;
    app.tab_rects = tab_rects;
    app.tab_close_rects = tab_close_rects;
    app.tab_prev_rect = tab_prev;
    app.tab_next_rect = tab_next;
    app.ws_rects = ws_rects;
    app.agent_rects = agent_rects;
    app.session_rects = session_rects;
    app.new_ws_rect = new_ws_rect;
}

// ── shared layout + state helpers (used across the ui submodules) ──

/// One cell inset on each side, for the border. Used to lay out the box.
fn pane_inner(rect: Rect, bordered: bool) -> Option<Rect> {
    if !bordered {
        if rect.width < 1 || rect.height < 1 {
            return None;
        }
        return Some(rect);
    }
    if rect.width < 4 || rect.height < 4 {
        return None;
    }
    Some(Rect::new(
        rect.x + 1,
        rect.y + 1,
        rect.width - 2,
        rect.height - 2,
    ))
}

/// Horizontal breathing room for a lone (border-less) pane, so its header and
/// terminal content line up with the tab bar's left edge (`area.x + 1`) instead
/// of touching the sidebar. Split panes get spacing from their borders instead.
pub(super) const LONE_PANE_HPAD: u16 = 1;

/// A footer hint line: each `(key, label)` rendered with the **key** in the
/// theme accent and the **label** in light text, separated by a dim `·`. Shared
/// by the modals (Settings / picker) and the git-tab footer. A pair with an
/// empty label is a bare key (e.g. `j/k`).
pub(super) fn hint_line(pairs: &[(&str, &str)], t: &Theme) -> Line<'static> {
    hint_line_with_offsets(pairs, t).0
}

/// Same rendering as [`hint_line`], plus each pair's start column within the
/// rendered line (so hitboxes can be aligned to the visible hints without
/// re-deriving the layout).
pub(super) fn hint_line_with_offsets(
    pairs: &[(&str, &str)],
    t: &Theme,
) -> (Line<'static>, Vec<u16>) {
    let mut spans = vec![Span::raw(" ")];
    let mut offsets = Vec::with_capacity(pairs.len());
    for (i, (key, label)) in pairs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", Style::new().fg(t.overlay0)));
        }
        // Column where this pair's key begins: width of everything before it
        // (leading pad + separators + earlier pairs).
        offsets.push(spans.iter().map(|span| span.width() as u16).sum());
        spans.push(Span::styled(
            key.to_string(),
            Style::new().fg(t.accent).bold(),
        ));
        if !label.is_empty() {
            spans.push(Span::styled(
                format!(" {label}"),
                Style::new().fg(t.subtext1),
            ));
        }
    }
    (Line::from(spans), offsets)
}

/// Display width of `s` in terminal columns (CJK = 2 cells, etc.). Fixed-width
/// chrome must measure with this, not `chars().count()`, so translated/CJK labels
/// align (docs/21).
pub(crate) fn display_width(s: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    s.width()
}

/// Truncate `s` to at most `max` display columns, ending in a `…` when it does not
/// fit. Width-aware like `display_width` (a CJK glyph counts as two, and is never
/// split), so a narrowed sidebar clips long node/agent/branch names gracefully
/// instead of hard-cutting mid-glyph (docs/29).
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if display_width(s) <= max {
        return s.to_string();
    }
    // Reserve one column for the ellipsis.
    let budget = max - 1;
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let cw = display_width(&ch.to_string());
        if used + cw > budget {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('…');
    out
}

/// A small centered toast box near the bottom (e.g. "✓ Copied"). Drawn last, so
/// it floats over everything; the loop clears it after ~1.4s.
fn draw_toast(f: &mut RenderTarget, area: Rect, text: &str, t: &Theme) {
    use ratatui::widgets::{Borders, Clear};
    let w = (display_width(text) as u16 + 6).min(area.width);
    let h = 3u16;
    if w < 6 || area.height < h + 3 {
        return;
    }
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.bottom().saturating_sub(h + 2); // just above the status line
    let rect = Rect::new(x, y, w, h);
    f.render_widget(Clear, rect);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(t.accent).bg(t.surface0))
        .style(Style::new().bg(t.surface0));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("✓ ", Style::new().fg(t.green)),
            Span::styled(text.to_string(), Style::new().fg(t.text).bold()),
        ]))
        .alignment(Alignment::Center),
        inner,
    );
}

/// The lone-pane horizontal pad, suppressed for panes too narrow to spare it.
pub(super) fn lone_pad(width: u16) -> u16 {
    if width > 2 * LONE_PANE_HPAD + 2 {
        LONE_PANE_HPAD
    } else {
        0
    }
}

/// The terminal content area: inside the box when bordered (the dot+path+close
/// live on the top border row as a title), else just below the header row with a
/// small horizontal pad so it aligns with the tab bar.
fn pane_content(rect: Rect, bordered: bool, mobile: bool) -> Option<Rect> {
    if bordered {
        return pane_inner(rect, true);
    }
    if mobile {
        return (rect.width > 0 && rect.height > 0).then_some(rect);
    }
    let pad = lone_pad(rect.width);
    let c = Rect::new(
        rect.x + pad,
        rect.y + 1,
        rect.width.saturating_sub(2 * pad),
        rect.height.saturating_sub(1),
    );
    if c.width < 1 || c.height < 1 {
        return None;
    }
    Some(c)
}

/// Rect of the ✕ close button at the right of a pane's top-border title row.
fn pane_close_rect(area: Rect, bordered: bool) -> Option<Rect> {
    if !bordered || area.width < 9 {
        return None;
    }
    Some(Rect::new(area.x + area.width - 4, area.y, 3, 1))
}

/// The focused pane's ⤢ zoom button, sitting three cells left of the ✕. Shown
/// only when the pane is wide enough to hold both without eating the title. Must
/// stay in lockstep with the button layout in [`panes::draw_pane_titles`].
fn pane_zoom_rect(area: Rect, bordered: bool) -> Option<Rect> {
    if !bordered || area.width < 12 {
        return None;
    }
    Some(Rect::new(area.x + area.width - 7, area.y, 3, 1))
}

/// The ⤡ restore button in a *lone* pane's header (a zoomed split, docs/18),
/// right-aligned inside the padded header row. Must match the render in
/// [`panes::draw_one_pane`].
pub(super) fn lone_zoom_rect(area: Rect) -> Rect {
    let pad = lone_pad(area.width);
    Rect::new(area.x + area.width.saturating_sub(pad + 3), area.y, 3, 1)
}

fn pane_state(app: &App, id: PaneId) -> State {
    app.status
        .get(&id)
        .map(|s| s.state)
        .unwrap_or(State::Unknown)
}

/// Collapse `$HOME` to `~` and truncate from the left to fit `max` columns.
fn short_path(p: &Path, max: u16) -> String {
    let mut s = p.display().to_string();
    if let Some(home) = crate::platform::home_dir() {
        if let Some(rest) = s.strip_prefix(home.to_string_lossy().as_ref()) {
            s = format!("~{rest}");
        }
    }
    let max = max as usize;
    if s.chars().count() > max && max > 1 {
        let tail: String = s
            .chars()
            .rev()
            .take(max - 1)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("…{tail}")
    } else {
        s
    }
}

#[cfg(test)]
mod bar_projection_tests {
    use super::*;

    #[test]
    fn secondary_viewport_preserves_active_bar_geometry_and_popup() {
        let _env = crate::persist::test_env("bar-projection");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(100, 30, tx).unwrap();
        let hit = crate::bar::BarHit {
            key: crate::bar::BarWidgetKey::new("module", "status"),
            segment: 0,
            rect: Rect::new(90, 29, 4, 1),
            action: "open".into(),
            value: Some("active".into()),
        };
        app.bar.hits = vec![hit.clone()];
        app.bar.overflow = Some(crate::bar::OverflowPopup {
            region: crate::bar::BarRegion::BottomRight,
            keys: vec!["module:hidden".into()],
            rect: Rect::new(70, 20, 24, 6),
        });
        let expected_popup = app.bar.overflow.as_ref().unwrap().rect;

        let area = Rect::new(0, 0, 60, 20);
        let mut buffer = Buffer::empty(area);
        let mut target = RenderTarget::new(&mut buffer, area);
        render_projection(&mut target, &mut app);

        assert_eq!(app.bar.hits, vec![hit]);
        assert_eq!(app.bar.overflow.as_ref().unwrap().rect, expected_popup);
    }

    #[test]
    fn mobile_projection_does_not_leak_profile_or_mobile_hits() {
        let _env = crate::persist::test_env("mobile-projection");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        app.config.layout.mobile_width = 80;
        app.open_switcher();

        let desktop_area = Rect::new(0, 0, 120, 40);
        let mut desktop_buffer = Buffer::empty(desktop_area);
        let mut desktop = RenderTarget::new(&mut desktop_buffer, desktop_area);
        render_into(&mut desktop, &mut app);
        assert!(!app.compact);
        assert!(app.switcher_close_rect.is_none());
        let desktop_content = app.pane_content_rects.clone();
        let focus = app.layout().focus;
        let pty_size = app.panes[&focus].size();

        let phone_area = Rect::new(0, 0, 79, 35);
        let mut phone_buffer = Buffer::empty(phone_area);
        let mut phone = RenderTarget::new(&mut phone_buffer, phone_area);
        render_projection(&mut phone, &mut app);

        assert!(
            !app.compact,
            "passive phone projection cannot replace desktop profile"
        );
        assert!(app.switcher_close_rect.is_none());
        assert_eq!(app.pane_content_rects, desktop_content);
        assert_eq!(app.panes[&focus].size(), pty_size);
    }
}
