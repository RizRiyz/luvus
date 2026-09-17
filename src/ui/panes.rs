//! Pane content: the terminal grid blit, the lone-pane header bar, and the
//! dot+path+close title drawn onto each split pane's top border.

use super::*;

/// Draw the dot + path (+ ✕ for the focused pane) as a title ON each pane's top
/// border row, after the borders are drawn, so it lands on the tab bar edge.
pub(super) fn draw_pane_titles(
    f: &mut RenderTarget,
    rects: &[(PaneId, Rect)],
    focus: PaneId,
    app: &App,
    t: &Theme,
) -> Vec<(PaneId, Rect)> {
    let mut title_rects = Vec::new();
    for (id, rect) in rects {
        if rect.width < 8 || rect.height < 2 {
            continue;
        }
        // A view leaf's title is its file path + a state dot placeholder.
        if let Some(view) = app.views.get(id) {
            let focused = *id == focus;
            let bg = t.mantle;
            let inner_w = rect.width - 2;
            let btn_w = title_buttons_w(focused, rect.width);
            let title_w = inner_w.saturating_sub(btn_w);
            let (marker, name) = match view {
                crate::app::ViewKind::File(v) => (
                    "■",
                    v.path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                ),
                crate::app::ViewKind::Diff(v) => (
                    crate::diff::DIFF_GLYPH,
                    format!("DIFF · {}", v.key.display_path()),
                ),
                crate::app::ViewKind::Preview(v) => (
                    "◇",
                    format!(
                        "{} · {}",
                        v.kind.label(),
                        v.path
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    ),
                ),
            };
            let path_fg = if focused { t.accent } else { t.subtext0 };
            // Plain terminal glyphs only: files use a square, DIFF uses its
            // dedicated filled triangle. Neither depends on emoji rendering.
            let dot = Span::styled(format!(" {marker} "), Style::new().fg(t.overlay1).bg(bg));
            let label: String = name
                .chars()
                .take(title_w.saturating_sub(3) as usize)
                .collect();
            let text_w = (3 + label.chars().count() as u16).min(title_w);
            let title_rect = Rect::new(rect.x + 1, rect.y, text_w, 1);
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    dot,
                    Span::styled(label, Style::new().fg(path_fg).bg(bg)),
                ])),
                title_rect,
            );
            title_rects.push((*id, title_rect));
            draw_title_buttons(f, *rect, focused, app.zoomed, title_w, bg, t);
            continue;
        }
        let Some(pane) = app.panes.get(id) else {
            continue;
        };
        let focused = *id == focus;
        let st = pane_state(app, *id);
        let path_fg = if focused { t.accent } else { t.subtext0 };
        // The top border is a thin rule now (not a filled bar), so the title
        // sits on the dark background; only the text cells are painted, leaving
        // the thin `▔` line visible on either side of the label.
        let bg = t.mantle;
        let inner_w = rect.width - 2; // inside the two corner cells
        let btn_w = title_buttons_w(focused, rect.width);
        let title_w = inner_w.saturating_sub(btn_w);
        // A named pane (via `pane name` / `agent name`) shows its name here; an
        // unnamed pane shows its cwd path. So naming a pane visibly renames it.
        // With `pane_title_path` on, a named pane shows `name  path` (both).
        let label = match app.agent_name_for(*id) {
            Some(name) if app.config.layout.pane_title_path => {
                let path = short_path(&pane.cwd, title_w.saturating_sub(4 + name.len() as u16 + 2));
                format!("{name}  {path}")
                    .chars()
                    .take(title_w.saturating_sub(4) as usize)
                    .collect::<String>()
            }
            Some(name) => name
                .chars()
                .take(title_w.saturating_sub(4) as usize)
                .collect::<String>(),
            None => short_path(&pane.cwd, title_w.saturating_sub(4)),
        };
        let text_w = (3 + label.chars().count() as u16).min(title_w);
        let title = Line::from(vec![
            Span::styled(
                format!(" {} ", st.dot()),
                Style::new().fg(st.color(t)).bg(bg),
            ),
            Span::styled(label, Style::new().fg(path_fg).bg(bg)),
        ]);
        let title_rect = Rect::new(rect.x + 1, rect.y, text_w, 1);
        f.render_widget(Paragraph::new(title), title_rect);
        // Keep the title geometry so clicks focus the pane and never become an
        // accidental divider resize on a stacked layout.
        title_rects.push((*id, title_rect));
        draw_title_buttons(f, *rect, focused, app.zoomed, title_w, bg, t);
    }
    title_rects
}

/// Cells reserved on the right of a focused pane's title for its buttons: the ✕,
/// plus the ⤢ zoom toggle when the pane is wide enough for both. Must match
/// `pane_close_rect`/`pane_zoom_rect` in `ui/mod.rs`, or a tap lands off the
/// glyph.
fn title_buttons_w(focused: bool, width: u16) -> u16 {
    if !focused {
        0
    } else if width >= 12 {
        6
    } else {
        3
    }
}

/// Draw the focused pane's title buttons at the right edge: ⤢/⤡ (zoom/restore)
/// then ✕, each a 3-cell hit target aligned with the rects `ui/mod.rs` records.
fn draw_title_buttons(
    f: &mut RenderTarget,
    rect: Rect,
    focused: bool,
    zoomed: bool,
    title_w: u16,
    bg: Color,
    t: &Theme,
) {
    if !focused {
        return;
    }
    let style = Style::new().fg(t.subtext1).bg(bg).bold();
    let bx = rect.x + 1 + title_w;
    let close = |f: &mut RenderTarget, x: u16| {
        f.render_widget(
            Paragraph::new(Span::styled(" × ", style)),
            Rect::new(x, rect.y, 3, 1),
        );
    };
    if rect.width >= 12 {
        // ⤢ expands a split to fullscreen; ⤡ restores it (touch-reachable zoom).
        let zoom = if zoomed { " ⤡ " } else { " ⤢ " };
        f.render_widget(
            Paragraph::new(Span::styled(zoom, style)),
            Rect::new(bx, rect.y, 3, 1),
        );
        close(f, bx + 3);
    } else {
        close(f, bx);
    }
}

// ── panes ─────────────────────────────────────────────────────────────────

struct PaneRenderContext<'a> {
    app: &'a App,
    diff_source_rects: &'a mut Vec<(PaneId, usize, crate::diff::DiffSide, Rect)>,
    diff_note_rects: &'a mut Vec<(PaneId, String, Rect)>,
    preview_link_rects: &'a mut Vec<(PaneId, String, Rect)>,
}

pub(super) fn draw_panes(
    f: &mut RenderTarget,
    rects: &[(PaneId, Rect)],
    bordered: bool,
    app: &mut App,
    t: &Theme,
) -> Option<(u16, u16, bool)> {
    let focus = app.layout().focus;
    let mut cursor = None;
    let mut diff_source_rects = Vec::new();
    let mut diff_note_rects = Vec::new();
    let mut preview_link_rects = Vec::new();
    {
        let mut context = PaneRenderContext {
            app,
            diff_source_rects: &mut diff_source_rects,
            diff_note_rects: &mut diff_note_rects,
            preview_link_rects: &mut preview_link_rects,
        };
        for (id, rect) in rects {
            if let Some(c) = draw_one_pane(f, *rect, *id, *id == focus, bordered, &mut context, t) {
                cursor = Some(c);
            }
        }
    }
    app.diff_source_rects = diff_source_rects;
    app.diff_note_rects = diff_note_rects;
    app.preview_link_rects = preview_link_rects;
    cursor
}

/// Patch only terminal rows captured by the VT damage ledger into a retained
/// client buffer. The caller has already proved that geometry and every
/// non-terminal layer are unchanged. Any uncertainty returns `Err(())` and the
/// server immediately uses the ordinary full renderer.
pub(super) fn patch_terminal_damage(
    f: &mut RenderTarget,
    app: &App,
    content_rects: &[(PaneId, Rect)],
    snapshots: &std::collections::HashMap<PaneId, crate::terminal::vt::DamageSnapshot>,
) -> Result<(), ()> {
    let leaves = app.layout().leaves();
    if leaves.len() != content_rects.len()
        || leaves.iter().any(|id| app.views.contains_key(id))
        || leaves
            .iter()
            .any(|id| !snapshots.contains_key(id) || !app.panes.contains_key(id))
    {
        return Err(());
    }

    let theme = &app.theme;
    let focus = app.layout().focus;
    let mut cursor = None;
    for id in leaves {
        let content = content_rects
            .iter()
            .find_map(|(candidate, rect)| (*candidate == id).then_some(*rect))
            .ok_or(())?;
        let snapshot = snapshots.get(&id).ok_or(())?;
        if snapshot.kind != crate::terminal::vt::DamageKind::Partial
            || snapshot.composer_region.is_some()
            || snapshot.scroll_offset != 0
            || app
                .status
                .get(&id)
                .is_some_and(|status| status.agent == "pi")
        {
            return Err(());
        }

        let blank = Style::new().bg(theme.mantle);
        let buffer = f.buffer_mut();
        let mut stack = [0u8; 4];
        let mut combined = String::new();
        for row in &snapshot.rows {
            if row.row >= content.height {
                continue;
            }
            let y = content.y + row.row;
            for x in content.x..content.x.saturating_add(content.width) {
                let cell = &mut buffer[(x, y)];
                cell.reset();
                cell.set_symbol(" ");
                cell.set_style(blank);
            }
            for cell in &row.cells {
                let style = terminal_cell_style(cell.style, theme, app.downsample);
                let symbol: &str = if cell.zero_width.is_empty() {
                    cell.character.encode_utf8(&mut stack)
                } else {
                    combined.clear();
                    combined.push(cell.character);
                    combined.extend(cell.zero_width.iter());
                    &combined
                };
                paint_terminal_cell(buffer, content, row.row, cell.column, symbol, style);
            }
        }

        if id == focus {
            cursor = pane_ime_cursor(content, snapshot.cursor);
        }
    }

    if let Some((x, y, visible)) = cursor {
        f.set_cursor_anchor(x, y, visible);
    }
    Ok(())
}

fn draw_one_pane(
    f: &mut RenderTarget,
    area: Rect,
    id: PaneId,
    focused: bool,
    bordered: bool,
    context: &mut PaneRenderContext<'_>,
    t: &Theme,
) -> Option<(u16, u16, bool)> {
    let app = context.app;
    // A view leaf (docs/38 FILE-3) renders natively, not from a PTY.
    if let Some(view) = app.views.get(&id) {
        let content = pane_content(area, bordered, app.compact)?;
        match view {
            crate::app::ViewKind::File(v) => {
                let sel = app.selection.filter(|s| s.pane == id);
                super::files::draw_file_view(f, content, v, sel.as_ref(), app.compact, t);
            }
            crate::app::ViewKind::Diff(v) => super::diff::draw_diff_view(
                f,
                content,
                id,
                v,
                super::diff::DiffRenderContext {
                    state: &app.diff,
                    picker: app.diff_agent_picker.as_ref(),
                    marker_style: app.config.layout.diff_marker_style,
                    color_mode: app.config.layout.diff_color_mode,
                    mobile: app.compact,
                    source_hits: context.diff_source_rects,
                    note_hits: context.diff_note_rects,
                },
                t,
            ),
            crate::app::ViewKind::Preview(v) => {
                let sel = app.selection.filter(|selection| selection.pane == id);
                context.preview_link_rects.extend(
                    super::preview::draw(f, content, v, sel.as_ref(), app.compact, t)
                        .into_iter()
                        .map(|(target, rect)| (id, target, rect)),
                );
            }
        }
        return None; // views own no terminal cursor
    }
    let pane = app.panes.get(&id)?;
    let st = pane_state(app, id);
    let content = pane_content(area, bordered, app.compact)?;

    // A lone pane has no border, so it shows a header bar on its top row.
    // Bordered panes instead get their dot+path+close as a title ON the top
    // border row (see `draw_pane_titles`), so it touches the tab bar.
    if !bordered && !app.compact {
        // Match the content's horizontal pad so the header bar aligns with the
        // tab bar and the terminal text below it.
        let pad = lone_pad(area.width);
        let header = Rect::new(area.x + pad, area.y, area.width.saturating_sub(2 * pad), 1);
        let hbg = if focused { t.surface1 } else { t.surface0 };
        let path_fg = if focused { t.accent } else { t.overlay1 };
        f.render_widget(Block::new().style(Style::new().bg(hbg)), header);
        // When this lone pane is a *zoomed* split (not just the only pane), show a
        // ⤡ restore button so a phone can un-zoom without a keyboard (docs/18).
        let show_restore = app.zoomed && header.width >= 8;
        let path_budget = header
            .width
            .saturating_sub(if show_restore { 8 } else { 5 });
        let title = Line::from(vec![
            Span::styled("▎", Style::new().fg(t.accent).bg(hbg)),
            Span::styled(
                format!(" {} ", st.dot()),
                Style::new().fg(st.color(t)).bg(hbg),
            ),
            Span::styled(
                short_path(&pane.cwd, path_budget),
                Style::new().fg(path_fg).bg(hbg),
            ),
        ]);
        f.render_widget(Paragraph::new(title), header);
        if show_restore {
            let r = super::lone_zoom_rect(area);
            f.render_widget(
                Paragraph::new(Span::styled(
                    " ⤡ ",
                    Style::new().fg(t.subtext1).bg(hbg).bold(),
                )),
                r,
            );
        }
    }

    // Content background = the dark pane background.
    f.render_widget(Block::new().style(Style::new().bg(t.mantle)), content);

    let downsample = app.downsample;
    // A mouse text-selection in this pane highlights its cells.
    let sel = app.selection.filter(|s| s.pane == id);
    // Keyboard copy selections live in absolute history coordinates. Resolve
    // those against the engine's current viewport inside the one render lock.
    let copy = app.copy_mode.filter(|copy| copy.pane == id);
    // The link under a `Ctrl`-held cursor (docs/58). Borrowed, not cloned: this
    // is the render path, and the spans are recomputed only when the hovered
    // cell changes anyway.
    let hover_link = app
        .hover_link
        .as_ref()
        .filter(|h| h.pane == id)
        .map(|h| &h.link);
    // The line a search jump landed on (docs/63): (content row, scroll offset it
    // was jumped to). Banded only while the view is unchanged, so any scroll or
    // new output hides it.
    let flash = app
        .search_flash
        .as_ref()
        .filter(|fl| fl.pane == id)
        .map(|fl| (fl.row, fl.scroll));
    let mut scrolled = 0usize;
    let agent = app.status.get(&id).map(|s| s.agent.as_str()).unwrap_or("");
    let is_codex = agent == "codex";
    let mut composer_region = None;
    let cursor_pos = match pane.engine.lock() {
        Ok(engine) => {
            let copy_top =
                copy.map(|_| engine.history_len().saturating_sub(engine.scroll_offset()));
            let selection_top = sel
                .and_then(|selection| selection.retained)
                .map(|_| engine.history_len().saturating_sub(engine.scroll_offset()));
            let cur = engine.cursor();
            let scan_pi = agent == "pi";
            let mut pi_caret: Option<(u16, u16)> = None;
            {
                let buf = f.buffer_mut();
                engine.for_each_cell(&mut |row, col, sym, cell| {
                    if row >= content.height || col >= content.width {
                        return;
                    }
                    if scan_pi && cell.mods.contains(ratatui::style::Modifier::REVERSED) {
                        pi_caret = Some(pick_bottom_left_caret(pi_caret, (row, col)));
                    }
                    let x = content.x + col;
                    let y = content.y + row;
                    let mut style = terminal_cell_style(cell, t, downsample);
                    // Highlight the cell if it's inside the mouse selection.
                    if sel.is_some_and(|selection| {
                        selection.retained.map_or_else(
                            || selection.contains(x, y),
                            |retained| {
                                selection_top.is_some_and(|top| {
                                    retained.contains(
                                        top.saturating_add(row as usize),
                                        col as usize,
                                        content.width as usize,
                                    )
                                })
                            },
                        )
                    }) {
                        style = style.bg(t.sel_bg);
                    }
                    if copy.is_some_and(|copy| {
                        copy_top.is_some_and(|top| {
                            copy.contains(top.saturating_add(row as usize), col as usize)
                        })
                    }) {
                        style = style.bg(t.sel_bg);
                    }
                    // The terminal's own cursor belongs to the child. During
                    // copy mode, draw Luvus's selection cursor instead.
                    if copy.is_some_and(|copy| {
                        copy_top.is_some_and(|top| {
                            copy.cursor == (top.saturating_add(row as usize), col as usize)
                        })
                    }) {
                        style = style.add_modifier(ratatui::style::Modifier::REVERSED);
                    }
                    // Underline the `Ctrl`-hovered link, so it reads as clickable
                    // before you commit to the click. Applied after the selection
                    // so a link inside selected text keeps both.
                    if hover_link.is_some_and(|l| l.covers(col, row)) {
                        style = style
                            .fg(t.accent)
                            .add_modifier(ratatui::style::Modifier::UNDERLINED);
                    }
                    paint_terminal_cell(buf, content, row, col, sym, style);
                });
            }
            scrolled = engine.scroll_offset();
            if is_codex {
                composer_region = engine.codex_composer_region();
            }
            if focused && copy.is_none() {
                if let Some((row, col)) = pi_caret {
                    Some((content.x + col, content.y + row, true))
                } else {
                    pane_ime_cursor(content, cur)
                }
            } else {
                None
            }
        }
        Err(_) => None,
    };

    if let Some(region) = composer_region {
        draw_codex_composer(
            f.buffer_mut(),
            content,
            region,
            t,
            app.config.theme == "quattro-rally",
        );
    }

    // The search-jump flash band (docs/63): recolor the landed row's background
    // full width, keeping the text, so it reads as a highlighted line. Only while
    // the pane is still at the offset we jumped to, so a scroll or new output
    // (which changes `scrolled`) hides it instead of banding the wrong line.
    if let Some((fr, fscroll)) = flash {
        if fr < content.height && scrolled == fscroll {
            let y = content.y + fr;
            let buf = f.buffer_mut();
            for x in content.x..content.right() {
                if let Some(c) = buf.cell_mut((x, y)) {
                    c.set_bg(t.sel_bg);
                }
            }
        }
    }

    // Scrollback indicator: when the viewport is above the live bottom, show how
    // far up (in lines) at the content's top-right so the state is never a
    // mystery. Any keystroke — or scrolling back down — returns to live.
    if scrolled > 0 && content.height > 0 {
        let label = format!(" ↑{scrolled} ");
        let w = crate::ui::display_width(&label) as u16;
        if w < content.width {
            let badge = Rect::new(content.x + content.width - w, content.y, w, 1);
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    label,
                    Style::new().fg(t.crust).bg(t.accent),
                ))),
                badge,
            );
        }
    }
    cursor_pos
}

/// In-view PTY cell, mapped into the pane. Hidden still returns a park so the
/// client can CUP after chrome.
fn pane_ime_cursor(content: Rect, cur: crate::terminal::vt::Cursor) -> Option<(u16, u16, bool)> {
    if content.width == 0 || content.height == 0 {
        return None;
    }
    if cur.x >= content.width || cur.y >= content.height {
        return None;
    }
    Some((content.x + cur.x, content.y + cur.y, cur.visible))
}

fn terminal_cell_style(
    cell: crate::terminal::vt::RenderCell,
    t: &Theme,
    downsample: bool,
) -> Style {
    let convert = |color: Color| {
        if downsample {
            crate::ipc::protocol::to_256(color)
        } else {
            color
        }
    };
    let foreground = if cell.fg == Color::Reset {
        t.text
    } else {
        convert(cell.fg)
    };
    let mut style = Style::new().fg(foreground);
    if !cell.mods.is_empty() {
        style = style.add_modifier(cell.mods);
    }
    if cell.bg != Color::Reset {
        style = style.bg(convert(cell.bg));
    }
    style
}

fn paint_terminal_cell(
    buf: &mut Buffer,
    content: Rect,
    row: u16,
    column: u16,
    symbol: &str,
    style: Style,
) {
    if row >= content.height || column >= content.width {
        return;
    }
    // Ratatui rejects C0/C1 text. The symbol is otherwise the complete
    // grapheme cluster, including combining marks and emoji joiners.
    let symbol = if symbol.starts_with(char::is_control) {
        " "
    } else {
        symbol
    };
    let x = content.x + column;
    let y = content.y + row;
    let target = &mut buf[(x, y)];
    target.set_symbol(symbol);
    target.set_style(style);

    // The engine omits a wide glyph's spacer cell. Preserve it as an empty
    // Ratatui symbol so the client does not print a space over the right half.
    if x + 1 < content.x + content.width && unicode_width::UnicodeWidthStr::width(symbol) == 2 {
        let next = &mut buf[(x + 1, y)];
        next.set_symbol("");
        next.set_style(style);
    }
}

/// Pi's `CURSOR_MARKER` (`ESC_pi:c BEL`) is stripped in `extractCursorPosition`
/// before the PTY write, so Luvus never sees a direct marker. Hidden PTY CUP is
/// often out of view or on the row tail while working. Bottom-most then leftmost
/// reverse-video cell in this pane is the fake caret (`ESC[7m`). Show the host
/// cursor there so IME preedit has a block; without this park the hardware
/// cursor stays on the last painted cell (the `working` spinner).
fn pick_bottom_left_caret(current: Option<(u16, u16)>, cell: (u16, u16)) -> (u16, u16) {
    match current {
        None => cell,
        Some((row, col)) => {
            let (r, c) = cell;
            if r > row || (r == row && c < col) {
                cell
            } else {
                (row, col)
            }
        }
    }
}

/// Give Codex's input a gently raised, theme-aware surface while retaining all
/// terminal text, foreground styling, and geometry.
fn draw_codex_composer(
    buf: &mut ratatui::buffer::Buffer,
    content: Rect,
    region: crate::terminal::vt::CodexComposerRegion,
    t: &Theme,
    subtle: bool,
) {
    if region.bottom < region.top || region.bottom >= content.height {
        return;
    }

    let top = content.y + region.top;
    let bottom = content.y + region.bottom;
    let fill = if subtle {
        t.subtle_composer_surface()
    } else {
        t.composer_surface()
    };

    for y in top..=bottom {
        for x in content.x..content.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_bg(fill);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::vt::CodexComposerRegion;

    #[test]
    fn composer_uses_only_a_subtle_theme_fill_and_preserves_geometry() {
        let t = Theme::quattro_rally();
        let area = Rect::new(0, 0, 20, 5);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        buf[(0, 2)].set_symbol("›");
        buf[(2, 2)].set_symbol("H");

        draw_codex_composer(
            &mut buf,
            area,
            CodexComposerRegion { top: 1, bottom: 3 },
            &t,
            true,
        );

        assert_eq!(buf[(0, 2)].symbol(), "›");
        assert_eq!(buf[(2, 2)].symbol(), "H");
        assert_eq!(buf[(0, 1)].symbol(), " ");
        assert_eq!(buf[(19, 3)].symbol(), " ");
        assert_eq!(buf[(10, 2)].bg, t.subtle_composer_surface());
        assert_ne!(buf[(10, 2)].bg, t.mantle);
        assert_ne!(buf[(10, 2)].bg, t.surface0);
    }

    fn cur(x: u16, y: u16, visible: bool) -> crate::terminal::vt::Cursor {
        crate::terminal::vt::Cursor { x, y, visible }
    }

    #[test]
    fn hidden_in_view_pty_is_parked() {
        let content = Rect::new(2, 3, 20, 12);
        assert_eq!(
            pane_ime_cursor(content, cur(4, 8, false)),
            Some((6, 11, false))
        );
    }

    #[test]
    fn visible_pty_caret_in_prompt_is_followed() {
        let content = Rect::new(0, 0, 20, 12);
        assert_eq!(
            pane_ime_cursor(content, cur(5, 10, true)),
            Some((5, 10, true))
        );
    }

    #[test]
    fn in_view_top_row_pty_is_followed() {
        let content = Rect::new(2, 3, 20, 12);
        assert_eq!(
            pane_ime_cursor(content, cur(4, 0, true)),
            Some((6, 3, true))
        );
    }

    #[test]
    fn out_of_view_pty_yields_none() {
        let content = Rect::new(0, 0, 20, 12);
        assert_eq!(pane_ime_cursor(content, cur(20, 0, true)), None);
        assert_eq!(pane_ime_cursor(content, cur(0, 12, false)), None);
    }

    #[test]
    fn pi_caret_prefers_bottom_then_left_reversed_cell() {
        assert_eq!(pick_bottom_left_caret(None, (3, 9)), (3, 9));
        assert_eq!(pick_bottom_left_caret(Some((3, 9)), (3, 2)), (3, 2));
        assert_eq!(pick_bottom_left_caret(Some((3, 2)), (5, 18)), (5, 18));
        assert_eq!(pick_bottom_left_caret(Some((5, 18)), (5, 4)), (5, 4));
        assert_eq!(pick_bottom_left_caret(Some((5, 4)), (4, 0)), (5, 4));
    }
}
