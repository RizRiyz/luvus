//! The sidebar: brand/Menu chrome plus a stack of **docks** (docs/29). The
//! built-in docks are WORKSPACES and AGENTS; `draw_sidebar` is a thin container
//! that lays out its dock list and dispatches to each dock's draw fn. With the
//! default list `[Workspaces, Agents]` the output is identical to the original
//! single-purpose left sidebar.

use super::*;

fn attention(s: State) -> u8 {
    match s {
        State::Blocked => 4,
        State::Done => 3,
        State::Working => 2,
        State::Idle => 1,
        State::Unknown => 0,
    }
}

/// Most urgent pane state across a whole workspace.
fn rollup(app: &App, ws_index: usize) -> State {
    let mut best = State::Idle;
    if let Some(ws) = app.workspaces.get(ws_index) {
        for tab in &ws.tabs {
            for id in tab.layout.leaves() {
                let s = pane_state(app, id);
                if attention(s) > attention(best) {
                    best = s;
                }
            }
        }
    }
    best
}

// ── sidebar ───────────────────────────────────────────────────────────────

/// Workspace, live-agent, scheduled-automation, resumable-session, and new-workspace hits.
pub(super) type SidebarHits = (
    Vec<(usize, Rect)>,
    Vec<(PaneId, Rect)>,
    Vec<(String, Rect)>,
    Vec<(usize, Rect)>,
    Option<Rect>,
);

/// Clickable geometry a single dock reports back to the container.
type WorkspaceHits = (Vec<(usize, Rect)>, Option<Rect>);
type AgentHits = (Vec<(PaneId, Rect)>, Vec<(String, Rect)>, Vec<(usize, Rect)>);

/// Rows of sidebar chrome above the dock stack: the brand/menu row plus one
/// blank separator row. The dock body, and therefore dock-height measurement
/// during a divider drag, starts this many rows below the sidebar origin.
pub(crate) const SIDEBAR_CHROME_ROWS: u16 = 2;

/// Rows each list item occupies: two content rows, drawn back-to-back.
const ROW_STRIDE: u16 = 2;

/// How many items fit in a list `rows` tall.
fn list_capacity(rows: u16) -> usize {
    (rows / ROW_STRIDE) as usize
}

/// A scrollbar on the sidebar's right edge, shown only when the list overflows
/// its area. Thin block glyphs keep the indicator lighter than a full-cell
/// background strip while remaining terminal-native: a faint one-eighth-cell
/// track carries a brighter thumb of the same narrow width, sized to the
/// visible fraction.
fn draw_scrollbar(
    f: &mut RenderTarget,
    track: Rect,
    total: usize,
    cap: usize,
    scroll: usize,
    t: &Theme,
) {
    if total <= cap || track.height == 0 {
        return;
    }
    let len = track.height as usize;
    let thumb = (len * cap / total).clamp(1, len);
    let span = total - cap;
    let pos = ((len - thumb) * scroll.min(span))
        .checked_div(span)
        .unwrap_or(0);
    let buf = f.buffer_mut();
    for i in 0..len {
        let on = i >= pos && i < pos + thumb;
        let cell = &mut buf[(track.x, track.y + i as u16)];
        cell.set_symbol("▕");
        cell.set_fg(if on { t.overlay1 } else { t.surface1 });
    }
}

/// Split a sidebar `body` rect into `n` stacked dock slots with a one-row
/// divider between each. Reduces to the legacy 50/50 split for two docks (the
/// divider is taken from the remainder, so `slot0 = body.height / n`).
/// Returns `(slots, divider_rows)`.
fn dock_slots(body: Rect, weights: &[u16]) -> (Vec<Rect>, Vec<u16>) {
    let n = weights.len();
    let mut slots = Vec::with_capacity(n);
    let mut dividers = Vec::new();
    if n == 0 {
        return (slots, dividers);
    }
    let bottom = body.bottom();
    let mut y = body.y;
    // Equal weights keep the legacy even split (`remaining / docks_left`), so a
    // sidebar that has never been dragged lays out to the cell it always did.
    // Unequal weights — including rendered heights stored after a drag — are
    // proportioned against the *content* rows still unallocated (remaining
    // minus the divider rows still to be drawn). That is the pool those
    // heights already sum to, so a second layout produces the same geometry
    // instead of stealing a row from an untouched dock to pay for a divider.
    let equal = weights.iter().all(|w| *w == weights[0]);
    for i in 0..n {
        let remaining = bottom.saturating_sub(y);
        let weight_left: u32 = weights[i..].iter().map(|w| u32::from(*w)).sum();
        let docks_left = (n - i) as u16;
        let pool = if equal {
            remaining
        } else {
            remaining.saturating_sub(docks_left.saturating_sub(1))
        };
        let share = (u32::from(pool) * u32::from(weights[i]))
            .checked_div(weight_left)
            .unwrap_or(0) as u16;
        // The drag keeps both sides of a divider at the floor, but weights are
        // relative and the body is not: a lopsided pair, or the same pair after
        // the terminal shrinks, can round a share down to nothing and hide a
        // dock. Hold the floor here too, while reserving what the docks below
        // still need (their own floor plus a divider row each) so honouring it
        // cannot starve them. On a body too small for every floor the
        // reservation shrinks the ceiling below the floor, and the `min` keeps
        // the clamp well-formed while the layout degrades evenly.
        let reserved = docks_left.saturating_sub(1) * (crate::app::MIN_DOCK_HEIGHT + 1);
        let ceiling = remaining.saturating_sub(reserved);
        let h = share.clamp(crate::app::MIN_DOCK_HEIGHT.min(ceiling), ceiling);
        slots.push(Rect::new(body.x, y, body.width, h));
        y += h;
        if i + 1 < n {
            dividers.push(y);
            y += 1;
        }
    }
    (slots, dividers)
}

/// A one-row horizontal rule between two stacked docks.
///
/// Drawn in `border`, the same colour a pane frame uses, so every rule in the
/// chrome belongs to one family and a theme that tints its borders (quattro-rally
/// gold, matrix green) tints this too.
fn draw_dock_divider(f: &mut RenderTarget, area: Rect, y: u16, t: &Theme) {
    let buf = f.buffer_mut();
    for x in (area.x + 1)..area.right().saturating_sub(1) {
        buf[(x, y)]
            .set_symbol("─")
            .set_style(Style::new().fg(t.border).bg(t.base));
    }
}

pub(super) fn draw_sidebar(
    f: &mut RenderTarget,
    side: Side,
    area: Rect,
    app: &mut App,
    t: &Theme,
) -> SidebarHits {
    f.render_widget(Block::new().style(Style::new().bg(t.base)), area);
    {
        // Edge separator (standard vertical rule): the left sidebar carries it on
        // its right edge, the right sidebar on its left edge. It doubles as the
        // draggable resize seam (docs/29) and brightens while hovered or dragged.
        let sep_x = match side {
            Side::Left => area.right().saturating_sub(1),
            Side::Right => area.x,
        };
        // Hovered or dragging, the seam lights up in `border_focus` — the same
        // colour a focused pane frame uses, since it is the same kind of handle.
        // At rest it stays `surface0`, which most palettes put *below* the sidebar
        // background so the edge reads as a groove rather than a drawn line.
        let seam_active = app.hover_sidebar == Some(side) || app.sidebar_resize == Some(side);
        let sep_fg = if seam_active {
            t.border_focus
        } else {
            t.surface0
        };
        let buf = f.buffer_mut();
        for y in area.top()..area.bottom() {
            buf[(sep_x, y)]
                .set_symbol("│")
                .set_style(Style::new().fg(sep_fg).bg(t.base));
        }
    }

    // Chrome (brand + Menu on the left; a lone collapse chevron on the right),
    // then the dock body below it.
    match side {
        Side::Left => draw_left_chrome(f, area, app, t),
        Side::Right => draw_right_chrome(f, area, app, t),
    }

    // The dock stack fills the sidebar below the chrome (a single top row + one
    // blank separator row). The body is inset by one column on the separator side
    // so a dock never paints over the edge rule; the dock draw fns stay
    // side-agnostic.
    let body_top = area.y.saturating_add(SIDEBAR_CHROME_ROWS);
    let (body_x, body_w) = match side {
        Side::Left => (area.x, area.width),
        Side::Right => (area.x + 1, area.width.saturating_sub(1)),
    };
    let body = Rect::new(
        body_x,
        body_top,
        body_w,
        area.bottom().saturating_sub(body_top),
    );
    let docks = app.sidebars.get(side).docks.clone();
    let (slots, dividers) = dock_slots(body, &app.sidebars.get(side).dock_weights());
    // Publish the rules so a press can grab one. Recomputed every frame, so a
    // sidebar that stops being drawn leaves no stale drag target behind.
    app.dock_dividers.retain(|(s, _, _)| *s != side);
    for (i, &dy) in dividers.iter().enumerate() {
        draw_dock_divider(f, body, dy, t);
        app.dock_dividers.push((side, i, dy));
    }

    let mut ws_rects = Vec::new();
    let mut agent_rects = Vec::new();
    let mut automation_rects = Vec::new();
    let mut session_rects = Vec::new();
    let mut new_ws_rect = None;
    for (kind, slot) in docks.iter().zip(slots) {
        match kind {
            DockKind::Workspaces => {
                let (w, n) = draw_workspaces_dock(f, slot, app, t);
                ws_rects = w;
                new_ws_rect = n;
            }
            DockKind::Agents => {
                let (a, scheduled, s) = draw_agents_dock(f, slot, app, t);
                agent_rects = a;
                automation_rects = scheduled;
                session_rects = s;
            }
            DockKind::Files => super::files::draw_files_dock(f, slot, app, t),
            DockKind::Module(id) => draw_module_dock(f, slot, id, app, t),
        }
    }

    (
        ws_rects,
        agent_rects,
        automation_rects,
        session_rects,
        new_ws_rect,
    )
}

/// The left sidebar's chrome, all on the **top row** (`area.y`, aligned with the
/// tab bar): the `«` collapse chevron at the left edge, then the active named
/// session, then the Menu pill at the right. Sets the session, Settings/Menu,
/// and sidebar-toggle hit geometry.
fn draw_left_chrome(f: &mut RenderTarget, area: Rect, app: &mut App, t: &Theme) {
    let cat = app.catalog;
    let hover = app.hover;
    let over = |rc: Rect| {
        hover
            .is_some_and(|(hc, hr)| hc >= rc.x && hc < rc.right() && hr >= rc.y && hr < rc.bottom())
    };
    // The `«` collapse button sits at the left edge of the top row — the exact
    // row + column the tab-bar's `»` reopen button uses when the sidebar is
    // hidden, so toggling it never makes the control jump. Click it (or ⌃Space b)
    // to hide the sidebar; the `»` brings it back.
    let toggle = Rect::new(area.x, area.y, 3.min(area.width), 1);
    app.sidebar_toggle_rect = Some(toggle);
    let chev_style = if over(toggle) {
        Style::new().fg(t.crust).bg(t.accent).bold()
    } else {
        Style::new().fg(t.accent).bg(t.surface0).bold()
    };
    f.render_widget(Paragraph::new(Span::styled(" « ", chev_style)), toggle);

    // Settings/Menu button — a labelled pill at the right of the top row (inverts
    // on hover) so it's an obvious, tappable control. Text beats a lone glyph for
    // discoverability.
    let menu_label = format!(" {} ", cat.menu);
    let menu_w = crate::ui::display_width(&menu_label) as u16;
    let menu = Rect::new(area.right().saturating_sub(menu_w + 1), area.y, menu_w, 1);

    // The active named session replaces the static product wordmark. It is a
    // bounded click target; long names truncate before the fixed Menu pill. Two
    // quiet cells separate it from the collapse chevron so the controls do not
    // read as one combined button.
    let session_x = toggle.right().saturating_add(2).min(menu.x);
    draw_named_session_button(f, area.y, session_x, menu.x, app, t);

    // Menu drawn after the wordmark so the pill always sits on top.
    let (fg, bg) = if over(menu) {
        (t.crust, t.accent)
    } else {
        (t.accent, t.surface1)
    };
    f.render_widget(
        Paragraph::new(Span::styled(menu_label, Style::new().fg(fg).bg(bg).bold())),
        menu,
    );
    app.settings_icon_rect = Some(menu);
}

/// The right sidebar's chrome: just a `»` collapse chevron at its top-right (no
/// brand or Menu — those live on the left). Sets `right_sidebar_toggle_rect`.
fn draw_right_chrome(f: &mut RenderTarget, area: Rect, app: &mut App, t: &Theme) {
    let hover = app.hover;
    let over = |rc: Rect| {
        hover.is_some_and(|(c, r)| c >= rc.x && c < rc.right() && r >= rc.y && r < rc.bottom())
    };
    // The `»` collapse sits on the top row (aligned with the tab bar) at the
    // right edge — the row + column the tab-bar's `«` reopen uses when the right
    // sidebar is hidden, so toggling never makes the control jump.
    let toggle = Rect::new(area.right().saturating_sub(3), area.y, 3.min(area.width), 1);
    app.right_sidebar_toggle_rect = Some(toggle);
    let style = if over(toggle) {
        Style::new().fg(t.crust).bg(t.accent).bold()
    } else {
        Style::new().fg(t.accent).bg(t.surface0).bold()
    };
    f.render_widget(Paragraph::new(Span::styled(" » ", style)), toggle);

    // If no rendered left sidebar claimed the chrome, surface both Menu and the
    // active session here so neither control is stranded (docs/29). Menu remains
    // at the sidebar's left edge and the session follows it toward the `»`
    // collapse control.
    if app.settings_icon_rect.is_none() {
        let label = format!(" {} ", app.catalog.menu);
        let w = crate::ui::display_width(&label) as u16;
        let chrome_left = area.x.saturating_add(2);
        let menu = Rect::new(chrome_left, area.y, w.min(area.width), 1);
        if menu.right() <= toggle.x {
            let (fg, bg) = if over(menu) {
                (t.crust, t.accent)
            } else {
                (t.accent, t.surface1)
            };
            f.render_widget(
                Paragraph::new(Span::styled(label, Style::new().fg(fg).bg(bg).bold())),
                menu,
            );
            app.settings_icon_rect = Some(menu);
            draw_named_session_button(f, area.y, menu.right().saturating_add(1), toggle.x, app, t);
        }
    }
}

/// Draw the active named-session label into the available chrome interval.
/// Both sidebars use this helper so the selector follows Menu when only the
/// right sidebar is mounted, while preserving identical truncation and hover
/// behavior on either side.
fn draw_named_session_button(
    f: &mut RenderTarget,
    y: u16,
    x: u16,
    right: u16,
    app: &mut App,
    t: &Theme,
) {
    let available = right.saturating_sub(x);
    let name = crate::ui::truncate(
        &crate::session::display_name(),
        available.saturating_sub(2) as usize,
    );
    // Symmetric padding makes the hover/open highlight read as a compact pill
    // without relying on a dropdown glyph or letting the text touch its edges.
    let label = format!(" {name} ");
    let width = (crate::ui::display_width(&label) as u16).min(available);
    let rect = Rect::new(x, y, width, 1);
    app.named_session_button_rect = (app.server_mode && rect.width > 0).then_some(rect);
    let hovered = app.hover.is_some_and(|(column, row)| {
        column >= rect.x && column < rect.right() && row >= rect.y && row < rect.bottom()
    });
    let style = if app.server_mode && (app.named_session_menu.is_some() || hovered) {
        Style::new().fg(t.crust).bg(t.accent).bold()
    } else {
        Style::new().fg(t.text).bold()
    };
    f.render_widget(Paragraph::new(Span::styled(label, style)), rect);
}

/// The WORKSPACES dock: node rows (state dot + name + branch + path), the `+`
/// new-workspace button, and a scrollbar. `area` is the dock slot; the header is
/// on `area.y`, the list below it.
fn draw_workspaces_dock(
    f: &mut RenderTarget,
    area: Rect,
    app: &mut App,
    t: &Theme,
) -> WorkspaceHits {
    let cat = app.catalog;
    let cx = area.x + 2;
    let cw = area.width.saturating_sub(3);
    let bar_col = area.right().saturating_sub(2);
    let line_at = |f: &mut RenderTarget, y: u16, line: Line| {
        if y < area.bottom() {
            f.buffer_mut().set_line(cx, y, &line, cw);
        }
    };
    let mut ws_rects = Vec::new();

    line_at(f, area.y, header(cat.workspaces, t));
    let new_ws_rect = if area.width >= 8 {
        let rect = Rect::new(area.right().saturating_sub(4), area.y, 3, 1);
        f.render_widget(
            Paragraph::new(Span::styled(
                " + ",
                Style::new().fg(t.accent).bg(t.sel_bg).bold(),
            )),
            rect,
        );
        Some(rect)
    } else {
        None
    };
    let nlist_top = area.y + 1;
    let nrows = area.height.saturating_sub(1);
    let ncap = list_capacity(nrows);
    let ntotal = app.workspaces.len();
    // Draw order groups each worktree under the node it branched from (docs/18
    // WT-4), so scroll positions index into this order, not raw creation order.
    let order = app.workspace_display_order();
    let active_pos = order
        .iter()
        .position(|(i, _)| *i == app.active_ws)
        .unwrap_or(0);
    // Auto-reveal the active workspace when it changes (cycle / new / resume), without
    // fighting wheel scrolling (which never changes `active_ws`).
    if app.active_ws != app.last_active_ws_shown {
        if active_pos < app.workspaces_scroll {
            app.workspaces_scroll = active_pos;
        } else if ncap > 0 && active_pos >= app.workspaces_scroll + ncap {
            app.workspaces_scroll = active_pos + 1 - ncap;
        }
        app.last_active_ws_shown = app.active_ws;
    }
    app.workspaces_scroll = app.workspaces_scroll.min(ntotal.saturating_sub(ncap));
    app.workspaces_area = Rect::new(area.x, nlist_top, area.width, nrows);
    let nscroll = app.workspaces_scroll;
    for (vi, (i, is_member)) in order.into_iter().skip(nscroll).take(ncap).enumerate() {
        let y = nlist_top + vi as u16 * ROW_STRIDE;
        let active = i == app.active_ws;
        ws_rects.push((i, Rect::new(area.x, y, area.width, 2)));
        let st = rollup(app, i);
        let ws = &app.workspaces[i];
        let terminal_cwd = app.workspace_terminal_cwd(i).unwrap_or(&ws.cwd);
        let name_style = if active {
            Style::new().fg(t.accent).bold()
        } else {
            Style::new().fg(t.subtext1)
        };
        // A linked worktree is nested under its parent checkout with a connector.
        let indent: u16 = if is_member { 2 } else { 0 };
        // Row 1: state dot + workspace name + git branch (dot aligned with "WORKSPACES").
        // On a narrow sidebar the name keeps priority: it is ellipsized only when
        // it can't share the row, and the branch is fitted (then ellipsized, then
        // dropped) into whatever space is left, so the row never hard-cuts.
        let avail = (cw as usize).saturating_sub(indent as usize + 2);
        let name_w = crate::ui::display_width(&ws.name);
        let (name_disp, branch_disp) = match &ws.branch {
            Some(b) => {
                let branch_seg = 2 + crate::ui::display_width(b); // "  branch"
                if name_w + branch_seg <= avail {
                    (ws.name.clone(), Some(b.clone()))
                } else if name_w + 4 <= avail {
                    (
                        ws.name.clone(),
                        Some(crate::ui::truncate(b, avail - name_w - 2)),
                    )
                } else {
                    (crate::ui::truncate(&ws.name, avail), None)
                }
            }
            None => (crate::ui::truncate(&ws.name, avail), None),
        };
        let mut line1: Vec<Span> = Vec::new();
        if is_member {
            line1.push(Span::styled("└ ", Style::new().fg(t.overlay0)));
        }
        line1.push(Span::styled(st.dot(), Style::new().fg(st.color(t))));
        line1.push(Span::raw(" "));
        line1.push(Span::styled(name_disp, name_style));
        if let Some(b) = &branch_disp {
            line1.push(Span::styled(
                format!("  {b}"),
                Style::new().fg(if active { t.green } else { t.overlay0 }),
            ));
        }
        line_at(f, y, Line::from(line1));
        // Row 2: the project path, indented under the name (extra for members).
        let pad = 2 + indent as usize;
        line_at(
            f,
            y + 1,
            Line::from(Span::styled(
                format!(
                    "{}{}",
                    " ".repeat(pad),
                    short_path(terminal_cwd, cw.saturating_sub(pad as u16))
                ),
                Style::new().fg(if active { t.subtext0 } else { t.overlay0 }),
            )),
        );
        if active {
            let buf = f.buffer_mut();
            for row in [y, y + 1] {
                for x in area.x..area.right().saturating_sub(1) {
                    buf[(x, row)].set_bg(t.sel_bg);
                }
            }
        }
    }
    draw_scrollbar(
        f,
        Rect::new(bar_col, nlist_top, 1, nrows),
        ntotal,
        ncap,
        nscroll,
        t,
    );
    (ws_rects, new_ws_rect)
}

/// The AGENTS dock: live agents then the on-disk resumable-session history as
/// one scrollable list, with an All/Active header filter. `area` is the dock
/// slot; the header is on `area.y`, the list below it.
fn draw_agents_dock(f: &mut RenderTarget, area: Rect, app: &mut App, t: &Theme) -> AgentHits {
    let cat = app.catalog;
    let cx = area.x + 2;
    let cw = area.width.saturating_sub(3);
    let bar_col = area.right().saturating_sub(2);
    let line_at = |f: &mut RenderTarget, y: u16, line: Line| {
        if y < area.bottom() {
            f.buffer_mut().set_line(cx, y, &line, cw);
        }
    };
    let mut agent_rects = Vec::new();
    let mut automation_rects = Vec::new();
    let mut session_rects = Vec::new();

    let aheader = area.y;
    line_at(f, aheader, header(cat.agents, t));
    // All/Active filter toggle, right-aligned in the header row. "All" shows the
    // session history too; "Active" shows only live agents.
    app.agents_filter_rects.clear();
    let active_only = app.agents_active_only;
    if area.width >= 22 {
        let segs = [
            (format!(" {} ", cat.all), false),
            (format!(" {} ", cat.active), true),
        ];
        let total: u16 = segs
            .iter()
            .map(|(l, _)| crate::ui::display_width(l) as u16)
            .sum();
        let mut x = area.right().saturating_sub(1 + total);
        for (label, val) in &segs {
            let (label, val) = (label.as_str(), *val);
            let w = crate::ui::display_width(label) as u16;
            let rect = Rect::new(x, aheader, w, 1);
            let style = if active_only == val {
                Style::new().fg(t.crust).bg(t.accent).bold()
            } else {
                Style::new().fg(t.overlay1).bg(t.surface1)
            };
            f.render_widget(Paragraph::new(Span::styled(label, style)), rect);
            app.agents_filter_rects.push((val, rect));
            x = x.saturating_add(w);
        }
    }
    // Workspace scope is controlled by prefix `A`, Settings → Keys, or an
    // agent/session row's context menu. It consumes no extra dock row.
    let scoped = app.agents_scope_active();
    let alist_top = aheader + 1;
    let arows = area.bottom().saturating_sub(alist_top);
    app.agents_area = Rect::new(area.x, alist_top, area.width, arows);

    let focus = app.layout().focus;
    // Live agents across every workspace/tab (real agents or panes with a session).
    // `(pane, workspace name, tab label)`. The tab label follows the tab bar: a
    // The row's second line is `workspace · mention`, where the mention is how you
    // delegate to the pane (`=name` or `=<id>`). The tab is intentionally dropped here
    // in favor of the pane token, which is what a script or delegation needs.
    let mut live: Vec<(PaneId, String)> = Vec::new();
    let mut blocked_elsewhere: Vec<PaneId> = Vec::new();
    for (wi, ws) in app.workspaces.iter().enumerate() {
        let visible = !scoped || wi == app.active_ws;
        for tab in &ws.tabs {
            for id in tab.layout.leaves() {
                if let Some(s) = app.status.get(&id) {
                    let is_agent = app.manifests.is_agent(&s.agent) || s.agent_session.is_some();
                    if !is_agent {
                        continue;
                    }
                    if visible {
                        live.push((id, ws.name.clone()));
                    } else if s.state == State::Blocked {
                        blocked_elsewhere.push(id);
                    }
                }
            }
        }
    }
    // Pinned agents (right-click → Pin) float to the top; a stable sort keeps the
    // rest in workspace/tab order. Skipped entirely when nothing is pinned, which
    // is the common case: the sort itself is cheap, but it does a hash lookup per
    // comparison and this runs on every frame the dock is drawn.
    if !app.pinned_agents.is_empty() {
        live.sort_by_key(|(id, _)| !app.pinned_agents.contains(id));
    }
    // Armed definitions appear in Active as lightweight placeholders. Their
    // hit target opens read-only automation detail rather than focusing a pane.
    // As soon as a due occurrence owns a live ORCH task, the ordinary pane row
    // replaces this projection.
    let mut scheduled: Vec<(
        String,
        String,
        String,
        u64,
        bool,
        Option<crate::automation::ActiveTargetState>,
    )> = app
        .automation
        .automations
        .iter()
        .filter_map(|automation| {
            if !automation.enabled {
                return None;
            }
            let (workspace_index, workspace) = app
                .workspaces
                .iter()
                .enumerate()
                .find(|(_, workspace)| workspace.id == automation.task.workspace_id)?;
            if scoped && workspace_index != app.active_ws {
                return None;
            }
            let live_run = app
                .automation
                .runs
                .iter()
                .rev()
                .find(|run| run.automation_id == automation.id && run.status.is_live());
            let pane_backed = live_run
                .and_then(|run| run.task_id.as_deref())
                .and_then(|task| app.orch.task(task))
                .and_then(|task| task.assignee)
                .is_some_and(|pane| app.panes.contains_key(&PaneId(pane)));
            if pane_backed
                || live_run.is_some_and(|run| {
                    matches!(
                        run.status,
                        crate::automation::RunStatus::Running
                            | crate::automation::RunStatus::Review
                    )
                })
            {
                return None;
            }
            let starting = live_run.is_some();
            let deadline = live_run
                .map(|run| run.scheduled_at)
                .or(automation.next_run_at)?;
            Some((
                automation.id.clone(),
                automation.task.agent_id.clone(),
                workspace.name.clone(),
                deadline,
                starting,
                app.automation
                    .active_target_states
                    .get(&automation.id)
                    .copied(),
            ))
        })
        .collect();
    scheduled.sort_by_key(|item| item.3);
    // Scope resumable history by workspace ownership too. Longest matching root
    // wins, matching pane-home semantics when workspaces are nested. Keep the
    // original indices so click-to-resume still addresses `app.resumable`.
    let session_owner = |cwd: &std::path::Path| {
        app.workspaces
            .iter()
            .enumerate()
            .filter(|(_, ws)| crate::platform::is_subpath(cwd, &ws.cwd))
            .max_by_key(|(_, ws)| ws.cwd.as_os_str().len())
            .map(|(wi, _)| wi)
    };
    let resumable_scoped: Vec<usize> = if scoped && !active_only {
        app.resumable
            .iter()
            .enumerate()
            .filter(|(_, session)| session_owner(&session.cwd) == Some(app.active_ws))
            .map(|(i, _)| i)
            .collect()
    } else {
        Vec::new()
    };
    // Preserve the allocation-free default path: unscoped All indexes the
    // existing history directly, while only the opt-in scoped projection builds
    // an index. Active does neither because history is hidden.
    let resumable_total = if active_only {
        0
    } else if scoped {
        resumable_scoped.len()
    } else {
        app.resumable.len()
    };
    let atotal = live.len() + scheduled.len() + resumable_total;
    // The overflow line is always visible while attention is hidden, even if no
    // local rows exist. Reserve exactly one terminal row for it.
    let has_elsewhere = scoped && !blocked_elsewhere.is_empty();
    app.agents_elsewhere_rect = None;
    let acap = list_capacity(arows.saturating_sub(u16::from(has_elsewhere)));
    app.agents_scroll = app.agents_scroll.min(atotal.saturating_sub(acap));
    let ascroll = app.agents_scroll;

    if atotal == 0 {
        line_at(
            f,
            alist_top,
            Line::from(Span::styled(
                if active_only {
                    cat.no_active_agents
                } else {
                    cat.no_agents_or_sessions
                },
                Style::new().fg(t.overlay0),
            )),
        );
    } else {
        for (vi, k) in (ascroll..atotal).take(acap).enumerate() {
            let y = alist_top + vi as u16 * ROW_STRIDE;
            if let Some((id, wsname)) = live.get(k) {
                // A live agent: runtime status + which workspace it runs in.
                let id = *id;
                let focused = id == focus;
                let st = pane_state(app, id);
                let agent = app
                    .status
                    .get(&id)
                    .map(|s| s.agent.clone())
                    .unwrap_or_default();
                let name_style = if focused {
                    Style::new().fg(t.accent).bold()
                } else {
                    Style::new().fg(t.subtext1)
                };
                agent_rects.push((id, Rect::new(area.x, y, area.width, 2)));
                // Working stays visually prominent without scheduling animation
                // frames while the agent is busy.
                let dot = st.dot();
                let label = format!(" {}  ", st.label());
                let prefix_w = crate::ui::display_width(dot) + crate::ui::display_width(&label);
                let agent = crate::ui::truncate(&agent, (cw as usize).saturating_sub(prefix_w));
                line_at(
                    f,
                    y,
                    Line::from(vec![
                        Span::styled(dot, Style::new().fg(st.color(t))),
                        Span::styled(label, Style::new().fg(st.color(t))),
                        Span::styled(agent, name_style),
                    ]),
                );
                // Row 2: project · tab · how to mention this pane, styled exactly
                // like a workspace's path row. It was pinned to `overlay0`, which
                // lands on `sel_bg` for the focused row and is then all but
                // unreadable — the same reason the workspaces dock brightens its
                // path when active. The trailing token is the pane's live alias
                // (`=name`, set by `agent name`) or its pane id (`=3`), so the
                // reader can paste the token directly into a delegation line.
                let mention = app
                    .agent_name_for(id)
                    .map(|n| format!("={n}"))
                    .unwrap_or_else(|| format!("={}", id.0));
                // When enabled, show the agent's live session title (its OSC
                // title, e.g. "Ship the desktop release…") in place of the meta
                // line; fall back to it when the agent set no useful title.
                let meta = app
                    .config
                    .layout
                    .agent_title
                    .then(|| app.pane_title(id))
                    .flatten()
                    .map(|ttl| format!("  {ttl}"))
                    .unwrap_or_else(|| format!("  {wsname} · {mention}"));
                line_at(
                    f,
                    y + 1,
                    Line::from(Span::styled(
                        crate::ui::truncate(&meta, cw as usize),
                        Style::new().fg(if focused { t.subtext0 } else { t.overlay0 }),
                    )),
                );
                if focused {
                    let buf = f.buffer_mut();
                    for row in [y, y + 1] {
                        for x in area.x..area.right().saturating_sub(1) {
                            buf[(x, row)].set_bg(t.sel_bg);
                        }
                    }
                }
            } else if let Some((automation, agent, workspace, deadline, starting, target_state)) =
                scheduled.get(k.saturating_sub(live.len()))
            {
                automation_rects.push((
                    automation.clone(),
                    Rect::new(area.x, y, area.width, ROW_STRIDE),
                ));
                let (status, status_color) = match target_state {
                    Some(crate::automation::ActiveTargetState::Restoring) => {
                        (cat.automation_restoring, t.amber)
                    }
                    Some(crate::automation::ActiveTargetState::NeedsRebind) => {
                        (cat.automation_needs_rebind, t.coral)
                    }
                    _ if *starting => (cat.automation_starting, t.accent),
                    _ => (cat.automation_scheduled, t.accent),
                };
                let label = format!(" {status}  ");
                let prefix_w = 1 + crate::ui::display_width(&label);
                line_at(
                    f,
                    y,
                    Line::from(vec![
                        Span::styled("◷", Style::new().fg(status_color)),
                        Span::styled(label, Style::new().fg(status_color)),
                        Span::styled(
                            crate::ui::truncate(agent, (cw as usize).saturating_sub(prefix_w)),
                            Style::new().fg(t.subtext1).bold(),
                        ),
                    ]),
                );
                line_at(
                    f,
                    y + 1,
                    Line::from(Span::styled(
                        crate::ui::truncate(
                            &format!("  {workspace} · UTC {}", super::format_utc(*deadline)),
                            cw as usize,
                        ),
                        Style::new().fg(t.overlay0),
                    )),
                );
            } else {
                // A resumable session discovered on disk — click to reopen.
                let visible_index = k - live.len() - scheduled.len();
                let si = if scoped {
                    resumable_scoped[visible_index]
                } else {
                    visible_index
                };
                let s = &app.resumable[si];
                let proj = s
                    .cwd
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("project");
                let row = Rect::new(area.x, y, area.width, 2);
                session_rects.push((si, row));
                let label = " resume  ";
                let prefix_w = 1 + crate::ui::display_width(label);
                let name = crate::ui::truncate(&s.agent, (cw as usize).saturating_sub(prefix_w));
                line_at(
                    f,
                    y,
                    Line::from(vec![
                        Span::styled("○", Style::new().fg(t.overlay1)),
                        Span::styled(label, Style::new().fg(t.overlay1)),
                        Span::styled(name, Style::new().fg(t.subtext0)),
                    ]),
                );
                line_at(
                    f,
                    y + 1,
                    Line::from(Span::styled(
                        crate::ui::truncate(&format!("  {proj}"), cw as usize),
                        Style::new().fg(t.overlay0),
                    )),
                );
                // Removing / reopening a session is on the row's right-click menu
                // (docs/28) — no per-row ✕ button.
            }
        }
        draw_scrollbar(
            f,
            Rect::new(
                bar_col,
                alist_top,
                1,
                arows.saturating_sub(u16::from(has_elsewhere)),
            ),
            atotal,
            acap,
            ascroll,
            t,
        );
    }

    if has_elsewhere {
        let y = area.bottom().saturating_sub(1);
        if y >= alist_top {
            let text = cat
                .blocked_elsewhere
                .replace("{n}", &blocked_elsewhere.len().to_string());
            let state = State::Blocked;
            let dot = state.dot();
            let prefix_width = crate::ui::display_width(dot) + 1;
            line_at(
                f,
                y,
                Line::from(vec![
                    Span::styled(dot, Style::new().fg(state.color(t))),
                    Span::raw(" "),
                    Span::styled(
                        crate::ui::truncate(&text, (cw as usize).saturating_sub(prefix_width)),
                        Style::new().fg(t.subtext0),
                    ),
                ]),
            );
            app.agents_elsewhere_rect =
                Some((blocked_elsewhere[0], Rect::new(area.x, y, area.width, 1)));
        }
    }

    (agent_rects, automation_rects, session_rects)
}

/// A module-contributed dock (docs/29, DOCK-4): a header (its cached title) and
/// one row per pushed item — an optional state dot + text. Rows with an `action`
/// are recorded in `app.module_dock_rects` so a click can invoke it. `area` is
/// the dock slot; header on `area.y`, rows below (one row each).
fn draw_module_dock(f: &mut RenderTarget, area: Rect, id: &str, app: &mut App, t: &Theme) {
    let cx = area.x + 2;
    let cw = area.width.saturating_sub(3);
    let line_at = |f: &mut RenderTarget, y: u16, line: Line| {
        if y < area.bottom() {
            f.buffer_mut().set_line(cx, y, &line, cw);
        }
    };
    let (title, rows) = match app.module_docks.get(id) {
        Some(d) => (d.title.clone(), d.rows.clone()),
        None => (id.to_string(), Vec::new()),
    };
    line_at(f, area.y, header(&title, t));
    let list_top = area.y + 1;
    let cap = area.height.saturating_sub(1) as usize;
    for (i, row) in rows.iter().take(cap).enumerate() {
        let y = list_top + i as u16;
        let mut spans: Vec<Span> = Vec::new();
        let mut prefix_w = 0usize;
        if let Some(dot) = &row.dot {
            let st = state_from_name(dot);
            spans.push(Span::styled(st.dot(), Style::new().fg(st.color(t))));
            spans.push(Span::raw(" "));
            prefix_w = 2;
        }
        let text = crate::ui::truncate(&row.text, (cw as usize).saturating_sub(prefix_w));
        spans.push(Span::styled(text, Style::new().fg(t.subtext1)));
        line_at(f, y, Line::from(spans));
        if row.action.is_some() {
            app.module_dock_rects
                .push((id.to_string(), i, Rect::new(area.x, y, area.width, 1)));
        }
    }
}

/// Map a module-supplied state name to a status `State` (else `Unknown`).
fn state_from_name(s: &str) -> State {
    match s {
        "working" => State::Working,
        "blocked" => State::Blocked,
        "done" => State::Done,
        "idle" => State::Idle,
        _ => State::Unknown,
    }
}

fn header(text: &str, t: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::new().fg(t.overlay1).bold(),
    ))
}

#[cfg(test)]
mod tests {
    /// Equal weights must lay out exactly as the former `remaining / docks_left`
    /// did, to the cell: every sidebar that has never been resized keeps the
    /// geometry it had before weights existed.
    #[test]
    fn equal_weights_reproduce_the_former_even_split() {
        let body = ratatui::layout::Rect::new(0, 0, 20, 30);
        let (slots, dividers) = super::dock_slots(body, &[1, 1, 1]);
        assert_eq!(
            slots.iter().map(|s| s.height).collect::<Vec<_>>(),
            vec![10, 9, 9]
        );
        assert_eq!(dividers, vec![10, 20]);
        // Every row is accounted for: slots plus one row per divider.
        let used: u16 = slots.iter().map(|s| s.height).sum::<u16>() + dividers.len() as u16;
        assert_eq!(used, body.height);
    }

    /// Rendered heights stored as weights after a drag must survive a second
    /// layout pass. The previous formula proportioned against remaining rows
    /// *including* dividers, so `[15, 8, 5]` on a 30-row body became `[16, 8, 4]`
    /// and grew a dock that was not part of the drag.
    #[test]
    fn persisted_heights_round_trip_through_layout() {
        let body = ratatui::layout::Rect::new(0, 0, 20, 30);
        let weights = [15_u16, 8, 5];
        let (slots, dividers) = super::dock_slots(body, &weights);
        assert_eq!(
            slots.iter().map(|s| s.height).collect::<Vec<_>>(),
            weights,
            "heights used as weights must reproduce themselves"
        );
        assert_eq!(dividers, vec![15, 24]);
        let (again, dividers_again) = super::dock_slots(body, &weights);
        assert_eq!(
            again.iter().map(|s| s.height).collect::<Vec<_>>(),
            weights,
            "a second pass must not drift"
        );
        assert_eq!(dividers_again, dividers);
    }

    /// A weighted split hands out the sidebar in proportion, and still spends
    /// every row.
    #[test]
    fn weights_split_the_sidebar_in_proportion() {
        let body = ratatui::layout::Rect::new(0, 0, 20, 31);
        let (slots, dividers) = super::dock_slots(body, &[2, 1]);
        assert_eq!(slots[0].height, 20, "twice the weight, twice the rows");
        assert_eq!(slots[1].height, 10);
        assert_eq!(dividers.len(), 1);
        let used: u16 = slots.iter().map(|s| s.height).sum::<u16>() + dividers.len() as u16;
        assert_eq!(used, body.height, "no row is lost to rounding");
    }

    /// Weights are relative but the body is not, so a lopsided pair — or an
    /// ordinary one after the terminal shrinks — could round a share down to
    /// nothing and hide a dock. Every dock keeps the floor the drag promises.
    #[test]
    fn a_lopsided_weight_still_keeps_every_dock_at_the_floor() {
        let floor = crate::app::MIN_DOCK_HEIGHT;
        let body = ratatui::layout::Rect::new(0, 0, 20, 20);
        let (slots, dividers) = super::dock_slots(body, &[1, 100]);
        assert_eq!(
            slots[0].height, floor,
            "the starved dock is held at the floor"
        );
        assert!(
            slots[1].height >= floor,
            "and the greedy one still fits: {slots:?}"
        );
        let used: u16 = slots.iter().map(|s| s.height).sum::<u16>() + dividers.len() as u16;
        assert_eq!(used, body.height, "no row is lost");

        // Three docks, with the middle one starved.
        let (slots, _) = super::dock_slots(body, &[100, 1, 100]);
        for slot in &slots {
            assert!(
                slot.height >= floor,
                "every dock keeps the floor: {slots:?}"
            );
        }
    }

    /// A body too small to give every dock its floor must still lay out and
    /// spend every row rather than panic on an inverted clamp.
    #[test]
    fn a_body_below_every_floor_degrades_instead_of_panicking() {
        let body = ratatui::layout::Rect::new(0, 0, 20, 5);
        let (slots, dividers) = super::dock_slots(body, &[1, 1]);
        assert_eq!(slots.len(), 2);
        let used: u16 = slots.iter().map(|s| s.height).sum::<u16>() + dividers.len() as u16;
        assert_eq!(used, body.height);
    }

    use crate::app::App;
    use crate::event::AppEvent;
    use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::{backend::TestBackend, buffer::Buffer, layout::Rect, Terminal};

    #[test]
    fn sidebar_scrollbar_is_thin_and_proportional() {
        let area = Rect::new(0, 0, 1, 6);
        let mut buffer = Buffer::empty(area);
        let theme = crate::ui::theme::by_name("quattro-rally");
        {
            let mut target = crate::ui::RenderTarget::new(&mut buffer, area);
            super::draw_scrollbar(&mut target, area, 6, 2, 0, &theme);
        }

        let symbols: Vec<&str> = (0..area.height)
            .map(|row| buffer.cell((0, row)).expect("scrollbar cell").symbol())
            .collect();
        assert_eq!(
            symbols,
            vec!["▕", "▕", "▕", "▕", "▕", "▕"],
            "the proportional thumb and track stay one-eighth of a cell wide"
        );
        assert_eq!(buffer.cell((0, 0)).expect("thumb cell").fg, theme.overlay1);
        assert_eq!(buffer.cell((0, 2)).expect("track cell").fg, theme.surface1);
        assert!(
            (0..area.height).all(|row| buffer
                .cell((0, row))
                .is_some_and(|cell| cell.style().bg == Some(ratatui::style::Color::Reset))),
            "the scrollbar must not paint a full-cell background strip"
        );
    }

    fn buffer_contains(term: &Terminal<TestBackend>, needle: &str) -> bool {
        let buf = term.backend().buffer();
        (0..buf.area.height).any(|r| {
            (0..buf.area.width)
                .map(|c| buf.cell((c, r)).map(|x| x.symbol()).unwrap_or(" "))
                .collect::<String>()
                .contains(needle)
        })
    }

    /// The column each agent row's state label starts at, for every row drawn.
    fn label_columns(term: &Terminal<TestBackend>, label: &str) -> Vec<u16> {
        let buf = term.backend().buffer();
        (0..buf.area.height)
            .filter_map(|r| {
                let row: String = (0..buf.area.width)
                    .map(|c| buf.cell((c, r)).map(|x| x.symbol()).unwrap_or(" "))
                    .collect();
                row.find(label).map(|i| i as u16)
            })
            .collect()
    }

    // The state icon sits in a fixed one-column slot, so the text after it must
    // start at the same column no matter which state is shown.
    /// The fg colour of the first cell of the row containing `needle`.
    fn fg_of_row(term: &Terminal<TestBackend>, needle: &str) -> Option<ratatui::style::Color> {
        let buf = term.backend().buffer();
        for r in 0..buf.area.height {
            let row: String = (0..buf.area.width)
                .map(|c| buf.cell((c, r)).map(|x| x.symbol()).unwrap_or(" "))
                .collect();
            if let Some(i) = row.find(needle) {
                return buf.cell((i as u16, r)).map(|c| c.style().fg.unwrap());
            }
        }
        None
    }

    // Regression: the agent row's second line showed a hardcoded `tab N` built
    // from the tab index, so renaming a tab (docs/28) left the sidebar stale.
    #[test]
    fn agent_row_shows_the_pane_delegation_token() {
        let _env = crate::persist::test_env("agent-mention");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        let id = app.layout().focus;
        app.status.get_mut(&id).unwrap().agent = "claude".into();
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();

        // Unnamed: the row's second line shows the pane token (how you mention it),
        // in place of the tab that used to sit here.
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert!(
            buffer_contains(&term, &format!("· ={}", id.0)),
            "an unnamed agent row shows its pane id token"
        );

        // Naming the pane switches the token to `=name`.
        app.agent_names.insert("worker".into(), id);
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert!(
            buffer_contains(&term, "· =worker"),
            "a named agent row shows =name"
        );
    }

    // A node/branch name too long for the sidebar is ellipsized, never hard-cut
    // mid-word (docs/29 — matters now that the sidebar can be dragged narrow).
    #[test]
    fn long_workspace_name_is_ellipsized_not_hard_cut() {
        let _env = crate::persist::test_env("sidebar-truncate");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        let long = "a-really-long-workspace-name-that-cannot-possibly-fit";
        app.workspaces[0].name = long.into();
        app.workspaces[0].branch = Some("feature/some-very-long-branch-name".into());
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        // Read just the sidebar columns (the default left sidebar is 26 wide); the
        // full name can appear untruncated in the wider status/tab chrome.
        let buf = term.backend().buffer();
        let sidebar: String = (0..buf.area.height)
            .flat_map(|r| (0..26).map(move |c| buf.cell((c, r)).map(|x| x.symbol()).unwrap_or(" ")))
            .collect();
        assert!(sidebar.contains('…'), "an over-long name shows an ellipsis");
        assert!(
            !sidebar.contains(long),
            "the full over-long name is never rendered in the sidebar"
        );
    }

    // The focused agent's "project · =pane" line sits on the selection
    // background, so it must use the same readable colour the workspaces dock
    // gives its path row. Pinned to `overlay0` it was almost invisible on green.
    #[test]
    fn focused_agent_meta_line_is_readable() {
        let _env = crate::persist::test_env("agent-meta-colour");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        let id = app.layout().focus;
        app.status.get_mut(&id).unwrap().agent = "claude".into();
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();

        let t = crate::ui::theme::by_name(&app.config.theme);
        let meta = fg_of_row(&term, &format!("· ={}", id.0)).expect("the agent meta row is drawn");
        assert_eq!(
            meta, t.subtext0,
            "the focused agent's meta line must match the workspace path colour"
        );
        assert_ne!(
            meta, t.overlay0,
            "overlay0 is unreadable on the selection bg"
        );
    }

    #[test]
    fn agent_state_icons_keep_the_label_aligned() {
        use crate::ui::theme::State;
        let _env = crate::persist::test_env("agent-icon-align");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        let id = app.layout().focus;
        app.status.get_mut(&id).unwrap().agent = "claude".into();
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();

        // Where the label lands for each state.
        let mut columns = Vec::new();
        for st in [State::Idle, State::Blocked, State::Working, State::Done] {
            app.status.get_mut(&id).unwrap().state = st;
            term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
            let cols = label_columns(&term, st.label());
            assert!(!cols.is_empty(), "the {st:?} row should be drawn");
            columns.extend(cols);
        }

        let distinct: std::collections::HashSet<u16> = columns.iter().copied().collect();
        assert_eq!(
            distinct.len(),
            1,
            "every state icon must leave the label in the same column, got {distinct:?}"
        );
    }

    #[test]
    fn agents_all_active_toggle_filters_history() {
        // Isolate config so a concurrent test's saved sidebar layout can't leak in
        // via the shared `LUVUS_HOME` env var (fresh temp → default docks).
        let _env = crate::persist::test_env("agents");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        // One resumable session in the on-disk history (no live agents by default).
        app.resumable = vec![crate::agent::SessionInfo {
            agent: "claude".into(),
            session_id: "abc".into(),
            cwd: std::path::PathBuf::from("/tmp/proj"),
            updated: std::time::SystemTime::UNIX_EPOCH,
        }];
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();

        // Default = All: retained sessions remain visible after a fresh start or
        // snapshot restore, so history never appears to have been lost.
        assert!(!app.agents_active_only);
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert_eq!(app.agents_filter_rects.len(), 2, "All/Active toggle drawn");
        assert!(buffer_contains(&term, "Active"), "toggle label present");
        assert!(
            buffer_contains(&term, "resume"),
            "All default shows session history"
        );

        // Clicking Active resets the filtered list and persists the choice.
        app.agents_scroll = 7;
        let active = app
            .agents_filter_rects
            .iter()
            .find(|(active, _)| *active)
            .map(|(_, rect)| *rect)
            .expect("Active filter target");
        app.handle_event(AppEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: active.x,
            row: active.y,
            modifiers: KeyModifiers::NONE,
        }));
        assert!(app.agents_active_only);
        assert_eq!(app.agents_scroll, 0);
        assert!(crate::config::load().agents_active_only);
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert!(
            !buffer_contains(&term, "resume"),
            "Active hides session history"
        );

        // Clicking the selected choice is a true no-op, including its scroll.
        app.agents_scroll = 5;
        app.handle_event(AppEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: active.x,
            row: active.y,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(app.agents_scroll, 5);

        // All follows the same path and restores resumable history.
        let all = app
            .agents_filter_rects
            .iter()
            .find(|(active, _)| !*active)
            .map(|(_, rect)| *rect)
            .expect("All filter target");
        app.handle_event(AppEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: all.x,
            row: all.y,
            modifiers: KeyModifiers::NONE,
        }));
        assert!(!app.agents_active_only);
        assert_eq!(app.agents_scroll, 0);
        assert!(!crate::config::load().agents_active_only);
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert!(buffer_contains(&term, "resume"));
    }

    #[test]
    fn agents_workspace_scope_filters_both_row_kinds_and_keeps_attention_global() {
        let _env = crate::persist::test_env("agents-workspace-scope");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();

        // One workspace starts unscoped; scope remains available through the
        // prefix command and every agent/session row menu.
        assert!(!app.agents_this_workspace);
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();

        let first_pane = app.layout().focus;
        app.status.get_mut(&first_pane).unwrap().agent = "claude".into();
        let first_root = crate::persist::config_dir().join("parent-project");
        std::fs::create_dir_all(&first_root).unwrap();
        app.workspaces[0].cwd = first_root.clone();
        let second_root = first_root.join("nested-project");
        std::fs::create_dir_all(&second_root).unwrap();
        assert!(app.create_workspace_at(second_root.clone()));
        let second_pane = app.layout().focus;
        app.status.get_mut(&second_pane).unwrap().agent = "codex".into();
        assert_eq!(app.active_ws, 1);

        let first_workspace = app.workspaces[0].id.clone();
        let second_workspace = app.workspaces[1].id.clone();
        for (name, agent_id, workspace_id) in [
            ("first schedule", "agent-first", first_workspace),
            ("second schedule", "agent-second", second_workspace),
        ] {
            app.automation
                .create(
                    crate::automation::CreateAutomation {
                        name: name.into(),
                        enabled: true,
                        trigger: crate::automation::Trigger::Once { at_utc: 100 },
                        target: crate::automation::AutomationTarget::NewWorker,
                        task: crate::automation::TaskTemplate {
                            title: name.into(),
                            prompt: "run the scheduled task".into(),
                            agent_id: agent_id.into(),
                            workspace_id,
                            mode: crate::orch::TaskWorkerMode::Workspace,
                            access: crate::automation::AutomationAccess::Workspace,
                            paths: vec![],
                            gate: None,
                        },
                        policy: crate::automation::AutomationPolicy::default(),
                    },
                    None,
                    1,
                )
                .unwrap();
        }

        app.resumable = vec![
            crate::agent::SessionInfo {
                agent: "kimi".into(),
                session_id: "second-history".into(),
                cwd: second_root.join("nested"),
                updated: std::time::SystemTime::UNIX_EPOCH,
            },
            crate::agent::SessionInfo {
                agent: "hermes".into(),
                session_id: "first-history".into(),
                cwd: first_root.join("nested"),
                updated: std::time::SystemTime::UNIX_EPOCH,
            },
        ];

        // All workspaces is the default. Workspace scope is not a third member
        // of the All/Active segmented control and consumes no extra row.
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert_eq!(app.agents_filter_rects.len(), 2);
        assert!(!buffer_contains(&term, "Here"));
        for visible in ["claude", "codex", "kimi", "hermes"] {
            assert!(
                buffer_contains(&term, visible),
                "{visible} is initially visible"
            );
        }
        assert_eq!(
            app.automation_rects.len(),
            2,
            "unscoped dock shows schedules from both workspaces"
        );

        app.agents_scroll = 4;
        app.open_agent_menu(crate::app::AgentTarget::Live(second_pane), 0, 0);
        app.agent_menu_action(crate::app::AgentMenuItem::ToggleWorkspaceScope);
        assert!(app.agents_this_workspace);
        assert!(!app.agents_active_only, "scope is independent of lifecycle");
        assert_eq!(app.agents_scroll, 0);
        assert!(crate::config::load().agents_this_workspace);

        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert!(buffer_contains(&term, "codex"));
        assert!(buffer_contains(&term, "kimi"));
        assert!(!buffer_contains(&term, "claude"));
        assert!(!buffer_contains(&term, "hermes"));
        assert_eq!(
            app.automation_rects
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            vec!["a2"],
            "workspace scope also filters scheduled automation rows"
        );
        assert!(app.agents_elsewhere_rect.is_none());

        // Lifecycle remains an independent second axis: Active hides the local
        // resumable row but leaves workspace scope selected.
        assert!(app.set_agents_filter(true));
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert!(app.agents_this_workspace);
        assert!(buffer_contains(&term, "codex"));
        assert!(!buffer_contains(&term, "kimi"));
        assert!(app.set_agents_filter(false));

        // A hidden blocked row still participates in global attention. The one
        // overflow line names the count and targets a pane the scope actually hid.
        app.status.get_mut(&first_pane).unwrap().state = crate::ui::theme::State::Blocked;
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert!(buffer_contains(&term, "1 blocked in other"));
        let (target, overflow) = app.agents_elsewhere_rect.expect("attention overflow");
        assert_eq!(target, first_pane);
        app.handle_event(AppEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: overflow.x,
            row: overflow.y,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(app.layout().focus, first_pane);
        assert!(
            app.agents_this_workspace,
            "jump does not silently widen the list"
        );

        // With one workspace, persisted scope still applies to resumable
        // history under that workspace root.
        app.workspaces.truncate(1);
        app.active_ws = 0;
        assert!(app.agents_this_workspace);
        assert!(app.agents_scope_active());
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert!(buffer_contains(&term, "hermes"));
        assert!(
            buffer_contains(&term, "kimi"),
            "after the nested workspace closes, its cwd belongs to the parent"
        );
        assert_eq!(
            app.automation_rects
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            vec!["a1"],
            "closed-workspace schedules do not leak into the remaining scope"
        );
    }
}

#[cfg(test)]
mod chrome_colour_tests {
    use crate::app::{App, Side};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// The sidebar's rules belong to the same colour family as a pane frame, so a
    /// theme that tints its borders (quattro-rally gold, matrix green) tints these
    /// too instead of leaving cold grey lines in a warm palette.
    ///
    /// Asserted against the theme's own values rather than literal hexes, so it
    /// holds for every palette and cannot rot when one is retuned.
    #[test]
    fn sidebar_rules_follow_the_theme_border() {
        let _env = crate::persist::test_env("side-rules");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        app.theme = crate::ui::theme::by_name("quattro-rally");
        let t = app.theme.clone();
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();

        let seam_x = app.sidebars.left.width.saturating_sub(1);
        let divider_y = (0..40u16)
            .find(|y| term.backend().buffer().cell((5, *y)).map(|c| c.symbol()) == Some("─"))
            .expect("a dock divider is drawn");

        // The dock separator uses `border`.
        assert_eq!(
            term.backend().buffer().cell((5, divider_y)).unwrap().fg,
            t.border,
            "the dock divider is drawn in the theme's border colour"
        );

        // At rest the seam stays `surface0`: most palettes put it *below* the
        // sidebar background, so the edge reads as a groove, not a drawn line.
        let seam =
            |term: &Terminal<TestBackend>| term.backend().buffer().cell((seam_x, 10)).unwrap().fg;
        assert_eq!(seam(&term), t.surface0, "the resting seam is a groove");

        // Hovered, it lights up in `border_focus`, like a focused pane frame.
        app.hover = Some((seam_x, 10));
        app.hover_sidebar = Some(Side::Left);
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert_eq!(
            seam(&term),
            t.border_focus,
            "the hovered resize seam uses the focus border colour"
        );
    }
}
