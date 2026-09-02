//! The folder-picker modal: choose a folder to open as a new workspace.
//! Browse the filesystem, pick an existing folder, or create a new one.

use super::*;
use crate::app::{FolderPicker, PickerHit, Row};
use crate::i18n::Catalog;
use ratatui::crossterm::event::KeyCode;
use ratatui::widgets::{Borders, Clear};

/// Draw the picker over a dimmed backdrop; returns the clickable row rects
/// (row index → rect) the input layer uses for mouse selection.
pub(super) fn draw_picker(
    f: &mut RenderTarget,
    area: Rect,
    p: &FolderPicker,
    mobile: bool,
    cat: &Catalog,
    t: &Theme,
) -> Vec<(PickerHit, Rect)> {
    dim_backdrop(f, area, t);

    let w = area.width.saturating_sub(6).clamp(46, 76).min(area.width);
    let h = area.height.saturating_sub(4).clamp(14, 26).min(area.height);
    let modal = if mobile {
        super::mobile::sheets::full_screen(area)
    } else {
        centered_rect(area, w, h)
    };
    f.render_widget(Clear, modal);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(t.border_focus).bg(t.surface0))
        .style(Style::new().bg(t.surface0));
    let inner = block.inner(modal);
    f.render_widget(block, modal);

    // Title + the path being browsed.
    f.render_widget(
        Paragraph::new(Span::styled(
            format!(" {}", cat.open_workspace),
            Style::new().fg(t.text).bold(),
        )),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let path = p.path.display().to_string();
    let path = trunc_tail(&path, inner.width.saturating_sub(2) as usize);
    f.render_widget(
        Paragraph::new(Span::styled(format!(" {path}"), Style::new().fg(t.accent))),
        Rect::new(inner.x, inner.y + 1, inner.width, 1),
    );
    hline(f, inner.x, inner.y + 2, inner.width, t);

    // Footer: the in-modal path input, new-folder input, an error, or key hints.
    let footer_y = inner.bottom().saturating_sub(1);
    let divider_y = if p.going_to.is_some() {
        footer_y.saturating_sub(2)
    } else {
        footer_y.saturating_sub(1)
    };
    hline(f, inner.x, divider_y, inner.width, t);
    let mut footer_hints: Vec<(PickerHit, Rect)> = Vec::new();
    if let Some(buf) = &p.going_to {
        let input_y = footer_y.saturating_sub(1);
        let label = format!(" {}: ", cat.act_go_to);
        let input = trunc_tail(
            buf,
            (inner.width as usize)
                .saturating_sub(display_width(&label))
                .saturating_sub(1),
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(label, Style::new().fg(t.subtext0)),
                Span::styled(input, Style::new().fg(t.accent).bold()),
                Span::styled("▏", Style::new().fg(t.accent)),
            ])),
            Rect::new(inner.x, input_y, inner.width, 1),
        );
        if let Some(e) = &p.error {
            let error = trunc_tail(e, inner.width.saturating_sub(2) as usize);
            f.render_widget(
                Paragraph::new(Span::styled(format!(" {error}"), Style::new().fg(t.coral))),
                Rect::new(inner.x, footer_y, inner.width, 1),
            );
        } else {
            f.render_widget(
                Paragraph::new(hint_line(
                    &[("⏎", cat.act_go_to), ("esc", cat.act_cancel)],
                    t,
                )),
                Rect::new(inner.x, footer_y, inner.width, 1),
            );
        }
    } else if let Some(buf) = &p.creating {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!(" {}: ", cat.act_new_folder),
                    Style::new().fg(t.subtext0),
                ),
                Span::styled(buf.clone(), Style::new().fg(t.accent).bold()),
                Span::styled("▏", Style::new().fg(t.accent)),
            ])),
            Rect::new(inner.x, footer_y, inner.width, 1),
        );
    } else if let Some(e) = &p.error {
        f.render_widget(
            Paragraph::new(Span::styled(
                format!(" error: {e}"),
                Style::new().fg(t.coral),
            )),
            Rect::new(inner.x, footer_y, inner.width, 1),
        );
    } else {
        // Key hints: the shortcut in the theme accent, the label in light text —
        // over the modal's own background (no black bar). `⏎` acts on the
        // highlighted row (open folder / open with worktree / `..` / descend).
        // Every hint is clickable; a click replays its key.
        let hints = [
            ("g", cat.act_go_to, KeyCode::Char('g')),
            ("↑↓", cat.act_move, KeyCode::Down),
            ("⏎", cat.act_select, KeyCode::Enter),
            ("←", cat.act_up, KeyCode::Left),
            ("n", cat.act_new_folder, KeyCode::Char('n')),
            (".", cat.act_show_hidden, KeyCode::Char('.')),
            ("esc", cat.act_cancel, KeyCode::Esc),
        ];
        let (hints_line, hint_x) = hint_line_with_offsets(&hints.map(|(k, l, _)| (k, l)), t);
        f.render_widget(
            Paragraph::new(hints_line),
            Rect::new(inner.x, footer_y, inner.width, 1),
        );
        for (i, (key, label, code)) in hints.iter().enumerate() {
            let x = inner.x.saturating_add(hint_x[i]);
            let available = inner.right().saturating_sub(x);
            if available == 0 {
                continue;
            }
            let w = (display_width(key) + 1 + display_width(label)).min(available as usize);
            footer_hints.push((PickerHit::Hint(*code), Rect::new(x, footer_y, w as u16, 1)));
        }
    }

    // The scrolling list: [Open this folder] · [Home] · [..] · folders · files.
    let list = Rect::new(
        inner.x + 1,
        inner.y + 3,
        inner.width.saturating_sub(2),
        divider_y.saturating_sub(inner.y + 3),
    );
    let avail = list.height.max(1) as usize;
    let scroll = p.cursor.saturating_sub(avail.saturating_sub(1));
    let mut rects = Vec::new();
    for (vi, i) in (scroll..p.row_count()).take(avail).enumerate() {
        let y = list.y + vi as u16;
        let row_rect = Rect::new(list.x, y, list.width, 1);
        let sel = i == p.cursor;
        if sel {
            fill_bg(f, row_rect, t.sel_bg);
        }
        // (icon, label, color). Folders navigate; files are dimmed + inert.
        let (icon, label, fg) = match p.row(i) {
            Row::OpenFolder => ("✓", cat.open_this_folder.to_string(), t.accent),
            Row::OpenWorktree => ("⎇", cat.open_with_worktree.to_string(), t.accent),
            Row::Home => ("⌂", cat.home.to_string(), t.accent),
            Row::Up => ("↑", "..".to_string(), t.subtext0),
            Row::Entry(idx) => {
                let e = &p.entries[idx];
                if e.is_dir {
                    ("▪", format!("{}/", e.name), t.text)
                } else {
                    ("·", e.name.clone(), t.overlay0)
                }
            }
        };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(if sel { "▸ " } else { "  " }, Style::new().fg(t.accent)),
                Span::styled(format!("{icon} "), Style::new().fg(fg)),
                Span::styled(
                    trunc_tail(&label, list.width.saturating_sub(5) as usize),
                    Style::new().fg(fg),
                ),
            ])),
            Rect::new(list.x, y, list.width, 1),
        );
        rects.push((PickerHit::Row(i), row_rect));
    }
    if !footer_hints.is_empty() {
        rects.extend(footer_hints);
    }
    rects.push((PickerHit::Modal, modal));
    rects
}

/// Truncate a string to `max` display columns, keeping the **tail** (the useful
/// end of a path) with a leading `…`. Width-aware like [`truncate`] (a CJK glyph
/// counts as two, and is never split), so a wide-glyph path can't overflow its
/// row and clip whatever renders after it. A zero budget yields nothing — never
/// the full string — so a caller whose budget collapsed can't overflow either.
fn trunc_tail(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if display_width(s) <= max {
        return s.to_string();
    }
    // Reserve one column for the ellipsis.
    let budget = max - 1;
    let mut used = 0;
    let mut tail: Vec<char> = Vec::new();
    for ch in s.chars().rev() {
        let cw = display_width(&ch.to_string());
        if used + cw > budget {
            break;
        }
        tail.push(ch);
        used += cw;
    }
    let tail: String = tail.into_iter().rev().collect();
    format!("…{tail}")
}

/// A tiny input modal: the new-worktree branch prompt (docs/18 WT). `error` is
/// shown in red (e.g. the branch is already checked out) so a failed create is
/// never a silent no-op.
pub(super) fn draw_worktree_prompt(
    f: &mut RenderTarget,
    area: Rect,
    buf: &str,
    error: Option<&str>,
    hover: Option<(u16, u16)>,
    cat: &Catalog,
    t: &Theme,
) -> (Option<Rect>, Option<Rect>) {
    dim_backdrop(f, area, t);
    let w = area.width.saturating_sub(6).clamp(36, 64).min(area.width);
    let modal = centered_rect(area, w, 6);
    f.render_widget(Clear, modal);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(t.border_focus).bg(t.surface0))
        .style(Style::new().bg(t.surface0));
    let inner = block.inner(modal);
    f.render_widget(block, modal);
    f.render_widget(
        Paragraph::new(Span::styled(
            format!(" {}", cat.new_git_worktree),
            Style::new().fg(t.text).bold(),
        )),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!(" {}: ", cat.branch), Style::new().fg(t.subtext0)),
            Span::styled(buf.to_string(), Style::new().fg(t.accent).bold()),
            Span::styled("▏", Style::new().fg(t.accent)),
        ])),
        Rect::new(inner.x, inner.y + 2, inner.width, 1),
    );
    // Bottom line: the error (red) if the last create failed — never a silent
    // no-op — else the clickable key hints.
    let bottom = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
    if let Some(e) = error {
        let e = trunc_tail(e, inner.width.saturating_sub(2) as usize);
        f.render_widget(
            Paragraph::new(Span::styled(format!(" {e}"), Style::new().fg(t.coral))),
            bottom,
        );
        (None, None) // no hint buttons while the error occupies the line
    } else {
        let (c, x) = footer_hints(f, bottom, cat.act_create, cat.act_cancel, hover, t);
        (Some(c), Some(x))
    }
}

/// The open-worktree list modal (docs/18 WT): every checkout of the repo from
/// `git worktree list` — branch (or short head when detached), path, and an
/// "open" badge when the checkout is already a workspace. ⏎ opens (or focuses)
/// the highlighted row, esc closes.
///
/// Returns the footer's ⏎/esc button rects (as the text-input modals do, for the
/// hover pills and the shared button routing) plus the clickable list rects: one
/// [`PickerHit::Row`] per rendered row, then [`PickerHit::Modal`] for the modal
/// body, so the input layer can tell a row from inert chrome from the backdrop.
pub(super) fn draw_worktree_open(
    f: &mut RenderTarget,
    area: Rect,
    list: &crate::app::WorktreeOpenList,
    hover: Option<(u16, u16)>,
    cat: &Catalog,
    t: &Theme,
) -> (Option<Rect>, Option<Rect>, Vec<(PickerHit, Rect)>) {
    dim_backdrop(f, area, t);
    let w = area.width.saturating_sub(6).clamp(46, 76).min(area.width);
    // Borders (2) + title (1) + gap (1) + hints (1) around the rows.
    let h = (list.entries.len() as u16)
        .saturating_add(5)
        .clamp(7, 20)
        .min(area.height);
    let modal = centered_rect(area, w, h);
    f.render_widget(Clear, modal);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(t.border_focus).bg(t.surface0))
        .style(Style::new().bg(t.surface0));
    let inner = block.inner(modal);
    f.render_widget(block, modal);
    f.render_widget(
        Paragraph::new(Span::styled(
            format!(" {}", cat.menu_open_worktree),
            Style::new().fg(t.text).bold(),
        )),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    let bottom_y = inner.bottom().saturating_sub(1);
    let listing = Rect::new(
        inner.x + 1,
        inner.y + 2,
        inner.width.saturating_sub(2),
        bottom_y.saturating_sub(inner.y + 2),
    );
    let avail = listing.height.max(1) as usize;
    let scroll = list.cursor.saturating_sub(avail.saturating_sub(1));
    let mut rects = Vec::new();
    for (vi, i) in (scroll..list.entries.len()).take(avail).enumerate() {
        let e = &list.entries[i];
        let y = listing.y + vi as u16;
        let row_rect = Rect::new(listing.x, y, listing.width, 1);
        let sel = i == list.cursor;
        // Hover speaks the app's pointer language (accent fill, dark text) — the
        // same as a context-menu row — while the keyboard cursor keeps its own
        // subtler `sel_bg`, so the pointer and the cursor stay tellable apart
        // when they sit on different rows.
        let hot =
            hover.is_some_and(|(hc, hr)| hr == y && hc >= row_rect.x && hc < row_rect.right());
        if hot {
            fill_bg(f, row_rect, t.accent);
        } else if sel {
            fill_bg(f, row_rect, t.sel_bg);
        }
        // ⌂ marks the main checkout, ⎇ a linked worktree; a detached checkout
        // is labelled by its short head instead of a branch.
        let icon = if e.is_main { "⌂" } else { "⎇" };
        let label = e
            .branch
            .clone()
            .unwrap_or_else(|| e.head.chars().take(8).collect());
        let badge = if e.open {
            format!(" ● {}", cat.worktree_already_open)
        } else {
            String::new()
        };
        // One shared budget for the whole row: the fixed chrome (cursor, icon,
        // gap, badge) comes off first; the label leads but is capped so the
        // path keeps at least a third of what's left. Otherwise a long branch
        // name would push the path's tail and the badge off the row.
        let prefix = format!("▸ {icon} ");
        let path_full = e.path.display().to_string();
        let room = (listing.width as usize)
            .saturating_sub(display_width(&prefix) + 2 + display_width(&badge));
        let path_reserve = (room / 3).min(display_width(&path_full));
        let label = truncate(&label, room.saturating_sub(path_reserve));
        let path = trunc_tail(&path_full, room.saturating_sub(display_width(&label)));
        // Over the accent fill every span switches to the dark ink the theme
        // pairs with it; the per-span colours only read on the plain background.
        let ink = |fg: Color| Style::new().fg(if hot { t.crust } else { fg });
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(if sel { "▸ " } else { "  " }, ink(t.accent)),
                Span::styled(format!("{icon} "), ink(t.accent)),
                Span::styled(label, ink(t.text)),
                Span::styled(format!("  {path}"), ink(t.subtext0)),
                Span::styled(badge, ink(t.accent)),
            ])),
            row_rect,
        );
        rects.push((PickerHit::Row(i), row_rect));
    }
    // Last, so a row wins the hit test and only a click on neither is outside.
    rects.push((PickerHit::Modal, modal));

    let bottom = Rect::new(inner.x, bottom_y, inner.width, 1);
    let (c, x) = footer_hints(f, bottom, cat.act_select, cat.act_cancel, hover, t);
    (Some(c), Some(x), rects)
}

/// The tab-rename modal (docs/28): a single text field pre-filled with the tab's
/// current name. Mirrors `draw_worktree_prompt` (no error line).
pub(super) fn draw_tab_rename(
    f: &mut RenderTarget,
    area: Rect,
    buf: &str,
    hover: Option<(u16, u16)>,
    cat: &Catalog,
    t: &Theme,
) -> (Option<Rect>, Option<Rect>) {
    draw_rename(f, area, cat.rename_tab, buf, hover, cat, t)
}

/// The workspace-rename modal: titled for a node. The on-disk folder is never
/// touched; this edits the label only.
pub(super) fn draw_ws_rename(
    f: &mut RenderTarget,
    area: Rect,
    buf: &str,
    hover: Option<(u16, u16)>,
    cat: &Catalog,
    t: &Theme,
) -> (Option<Rect>, Option<Rect>) {
    draw_rename(f, area, cat.menu_rename, buf, hover, cat, t)
}

/// The pane-rename modal (same look as the workspace/tab rename).
pub(super) fn draw_pane_rename(
    f: &mut RenderTarget,
    area: Rect,
    buf: &str,
    hover: Option<(u16, u16)>,
    cat: &Catalog,
    t: &Theme,
) -> (Option<Rect>, Option<Rect>) {
    draw_rename(f, area, cat.menu_rename, buf, hover, cat, t)
}

/// Shared single-field rename modal (tab / workspace): a title, an editable
/// buffer, and the clickable ⏎/esc footer hints. Returns each hint's rect.
fn draw_rename(
    f: &mut RenderTarget,
    area: Rect,
    title: &str,
    buf: &str,
    hover: Option<(u16, u16)>,
    cat: &Catalog,
    t: &Theme,
) -> (Option<Rect>, Option<Rect>) {
    dim_backdrop(f, area, t);
    let w = area.width.saturating_sub(6).clamp(36, 64).min(area.width);
    let modal = centered_rect(area, w, 6);
    f.render_widget(Clear, modal);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(t.border_focus).bg(t.surface0))
        .style(Style::new().bg(t.surface0));
    let inner = block.inner(modal);
    f.render_widget(block, modal);
    f.render_widget(
        Paragraph::new(Span::styled(
            format!(" {title}"),
            Style::new().fg(t.text).bold(),
        )),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(" "),
            Span::styled(buf.to_string(), Style::new().fg(t.accent).bold()),
            Span::styled("▏", Style::new().fg(t.accent)),
        ])),
        Rect::new(inner.x, inner.y + 2, inner.width, 1),
    );
    let footer = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
    let (c, x) = footer_hints(f, footer, cat.act_save, cat.act_cancel, hover, t);
    (Some(c), Some(x))
}

/// A titled single-field prompt with an optional error line (docs/38 FILE-6):
/// the file-tree create/rename modal. Same look as the rename modal, plus a red
/// error row that keeps the prompt open on a failed create.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_rename_titled(
    f: &mut RenderTarget,
    area: Rect,
    title: &str,
    buf: &str,
    err: Option<&str>,
    hover: Option<(u16, u16)>,
    cat: &Catalog,
    t: &Theme,
) -> (Option<Rect>, Option<Rect>) {
    dim_backdrop(f, area, t);
    let w = area.width.saturating_sub(6).clamp(36, 70).min(area.width);
    let modal = centered_rect(area, w, 7);
    f.render_widget(Clear, modal);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(t.border_focus).bg(t.surface0))
        .style(Style::new().bg(t.surface0));
    let inner = block.inner(modal);
    f.render_widget(block, modal);
    f.render_widget(
        Paragraph::new(Span::styled(
            format!(" {title}"),
            Style::new().fg(t.text).bold(),
        )),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(" "),
            Span::styled(buf.to_string(), Style::new().fg(t.accent).bold()),
            Span::styled("▏", Style::new().fg(t.accent)),
        ])),
        Rect::new(inner.x, inner.y + 2, inner.width, 1),
    );
    // Error row (or the footer sits one row higher).
    let footer_y;
    if let Some(e) = err {
        f.render_widget(
            Paragraph::new(Span::styled(format!(" {e}"), Style::new().fg(t.coral))),
            Rect::new(inner.x, inner.y + 3, inner.width, 1),
        );
        footer_y = inner.bottom().saturating_sub(1);
    } else {
        footer_y = inner.bottom().saturating_sub(1);
    }
    let footer = Rect::new(inner.x, footer_y, inner.width, 1);
    let (c, x) = footer_hints(f, footer, cat.act_save, cat.act_cancel, hover, t);
    (Some(c), Some(x))
}

/// Render the footer `⏎ commit · esc cancel` hints (the original left-aligned
/// look) and return each hint's clickable rect, so a click drives the same
/// commit / cancel as the key. The hint under the cursor gets a subtle highlight.
fn footer_hints(
    f: &mut RenderTarget,
    row: Rect,
    commit: &str,
    cancel: &str,
    hover: Option<(u16, u16)>,
    t: &Theme,
) -> (Rect, Rect) {
    // Each hint is padded by a space each side, so its hover pill is a little
    // wider than the text and reads as a proper button.
    let cw = super::display_width(&format!("⏎ {commit}")) as u16 + 2;
    let xw = super::display_width(&format!("esc {cancel}")) as u16 + 2;
    let commit_rect = Rect::new(row.x, row.y, cw.min(row.width), 1);
    let sep_x = row.x + cw; // the `·` sits between the two pills' padding
    let cancel_x = (sep_x + 1).min(row.right());
    let cancel_rect = Rect::new(
        cancel_x,
        row.y,
        xw.min(row.right().saturating_sub(cancel_x)),
        1,
    );
    let over = |r: Rect| hover.is_some_and(|(c, hr)| c >= r.x && c < r.right() && hr == r.y);
    draw_hint(f, commit_rect, "⏎", commit, over(commit_rect), t);
    if sep_x < row.right() {
        f.render_widget(
            Paragraph::new(Span::styled("·", Style::new().fg(t.overlay0))),
            Rect::new(sep_x, row.y, 1, 1),
        );
    }
    draw_hint(f, cancel_rect, "esc", cancel, over(cancel_rect), t);
    (commit_rect, cancel_rect)
}

/// One footer hint ` ⏎ label `. When `hot`, the whole padded pill fills with the
/// theme accent (dark text on green); otherwise the key is the accent and the
/// label is light text, over the modal background (the original look).
fn draw_hint(f: &mut RenderTarget, rect: Rect, key: &str, label: &str, hot: bool, t: &Theme) {
    if hot {
        fill_bg(f, rect, t.accent);
    }
    let (kfg, lfg) = if hot {
        (t.crust, t.crust)
    } else {
        (t.accent, t.subtext1)
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(" "),
            Span::styled(key.to_string(), Style::new().fg(kfg).bold()),
            Span::styled(format!(" {label} "), Style::new().fg(lfg)),
        ])),
        rect,
    );
}

// ── local render helpers (each modal module keeps its own, as elsewhere) ──

fn centered_rect(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

/// Dim the whole frame toward `crust` so the dialog reads as focused.
fn dim_backdrop(f: &mut RenderTarget, area: Rect, t: &Theme) {
    let buf = f.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let cell = &mut buf[(x, y)];
            cell.set_fg(t.overlay0);
            cell.set_bg(t.crust);
        }
    }
}

fn hline(f: &mut RenderTarget, x: u16, y: u16, w: u16, t: &Theme) {
    let buf = f.buffer_mut();
    for i in 0..w {
        buf[(x + i, y)]
            .set_symbol("─")
            .set_style(Style::new().fg(t.surface1).bg(t.surface0));
    }
}

fn fill_bg(f: &mut RenderTarget, rect: Rect, color: Color) {
    let buf = f.buffer_mut();
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            buf[(x, y)].set_bg(color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trunc_tail_measures_display_columns() {
        // ASCII: keep the tail, lead with `…`.
        assert_eq!(trunc_tail("abcdef", 10), "abcdef");
        assert_eq!(trunc_tail("abcdef", 4), "…def");
        // A collapsed budget yields nothing (never the full string), and a
        // one-column budget fits only the ellipsis.
        assert_eq!(trunc_tail("abcdef", 0), "");
        assert_eq!(trunc_tail("abcdef", 1), "…");
        // CJK: each glyph is two columns; the result must fit the column
        // budget, not the char count, and never split a wide glyph.
        let s = "宽".repeat(10); // 20 columns
        let cut = trunc_tail(&s, 9);
        assert!(cut.starts_with('…'));
        assert!(
            display_width(&cut) <= 9,
            "{} columns leak past the budget",
            display_width(&cut)
        );
        // 9 columns = 1 (…) + an 8-column budget: exactly four 2-column glyphs.
        assert_eq!(cut, format!("…{}", "宽".repeat(4)));
        // An 8-column cap leaves a 7-column budget; the fourth glyph would need
        // 8, so it's dropped whole rather than split (7 used, one column spare).
        assert_eq!(trunc_tail(&s, 8), format!("…{}", "宽".repeat(3)));
    }
}
