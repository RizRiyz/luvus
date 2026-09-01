//! The FILES dock renderer (docs/38 FILE-1). Draws the flattened file tree; the
//! model in `crate::files` owns the state, this only paints it and records the
//! clickable rect per row. O(visible rows): it slices the flattened list to the
//! viewport and draws that, nothing more.

use crate::git::local::ChangeKind;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::files::{FileLoad, FileView, SIZE_CAP};
use crate::ui::theme::Theme;
use crate::ui::RenderTarget;

fn diff_note_count(count: usize) -> String {
    match count {
        0 => String::new(),
        1 => "  1 note".to_string(),
        count => format!("  {count} notes"),
    }
}

pub(super) fn draw_files_dock(f: &mut RenderTarget, area: Rect, app: &mut App, t: &Theme) {
    app.files_area = area;
    app.file_tree_rects.clear();
    app.files_mode_rects.clear();
    app.diff_row_rects.clear();

    let cx = area.x + 2;
    let cw = area.width.saturating_sub(3);
    // Write the row straight into the buffer with `set_line` (width + unicode
    // handled) instead of a `Paragraph` widget per row — cheaper on the docks'
    // hot path, which draw one styled line per row every frame.
    let line_at = |f: &mut RenderTarget, y: u16, line: Line| {
        if y < area.bottom() {
            f.buffer_mut().set_line(cx, y, &line, cw);
        }
    };

    // FILES and DIFF are modes of one dock. Keep their hit rectangles owned by
    // this renderer so secondary viewport projection cannot leak geometry.
    let files_w = 7u16.min(cw);
    let diff_w = 6u16.min(cw.saturating_sub(files_w));
    let files_rect = Rect::new(cx, area.y, files_w, 1);
    let diff_rect = Rect::new(cx.saturating_add(files_w), area.y, diff_w, 1);
    app.files_mode_rects
        .push((crate::diff::FilesMode::Files, files_rect));
    app.files_mode_rects
        .push((crate::diff::FilesMode::Diff, diff_rect));
    let mode_style = |mode| {
        if app.files_mode == mode {
            Style::new().fg(t.accent).bold()
        } else {
            Style::new().fg(t.overlay1)
        }
    };
    f.buffer_mut().set_line(
        files_rect.x,
        files_rect.y,
        &Line::from(Span::styled(
            "FILES  ",
            mode_style(crate::diff::FilesMode::Files),
        )),
        files_rect.width,
    );
    f.buffer_mut().set_line(
        diff_rect.x,
        diff_rect.y,
        &Line::from(Span::styled(
            "DIFF",
            mode_style(crate::diff::FilesMode::Diff),
        )),
        diff_rect.width,
    );

    // Workspace and branch are already present in Luvus chrome. Start content
    // immediately below the selector instead of spending a dock row repeating
    // that identity and DIFF progress.
    let list_top = area.y + 1;
    let cap = area.height.saturating_sub(1) as usize;
    if app.files_mode == crate::diff::FilesMode::Diff {
        draw_diff_list(f, area, list_top, cap, app, t, &line_at);
        return;
    }
    // Clamp scroll first (mutates `file_tree`), *then* borrow the memoized rows —
    // `visible_rows` returns a slice borrowing `file_tree`, so it must come after
    // the scroll write.
    let n = app.file_tree.visible_rows().len();
    app.file_tree.cursor = app.file_tree.cursor.min(n.saturating_sub(1));
    if app.files_focused && cap > 0 {
        if app.file_tree.cursor < app.file_tree.scroll {
            app.file_tree.scroll = app.file_tree.cursor;
        } else if app.file_tree.cursor >= app.file_tree.scroll.saturating_add(cap) {
            app.file_tree.scroll = app.file_tree.cursor.saturating_add(1).saturating_sub(cap);
        }
    }
    let max_scroll = n.saturating_sub(cap);
    app.file_tree.scroll = app.file_tree.scroll.min(max_scroll);
    let scroll = app.file_tree.scroll;
    let hover = app.hover;
    let keyboard_cursor = app.files_focused.then_some(app.file_tree.cursor);

    let rows = app.file_tree.visible_rows();
    for (i, row) in rows.iter().enumerate().skip(scroll).take(cap) {
        let y = list_top + (i - scroll) as u16;
        let rect = Rect::new(area.x, y, area.width, 1);
        let hovered = hover.is_some_and(|(hc, hr)| {
            hc >= rect.x && hc < rect.right() && hr >= rect.y && hr < rect.bottom()
        });
        let selected = keyboard_cursor == Some(i);

        // Indentation, then a marker column: a dir gets its expand chevron, a
        // file gets a small dot in the same column. A file used to render two
        // spaces there, so it read as a gap rather than a leaf of the tree.
        //
        // All three glyphs are one cell wide (`▾ ▸ •`), which is what keeps file
        // names aligned under folder names; a wider square like `◾` is two cells
        // and would shift every file row right by one. `•` (U+2022) is deliberate
        // over a filled square (`▪`), which out-weighs the thin chevron beside it,
        // and renders solidly in every font, unlike the hairline hollow shapes
        // (`▫`, `◦`) that can wash out once the dim marker colour is applied.
        let indent = "  ".repeat(row.depth as usize);
        let glyph = if row.is_dir {
            if row.expanded {
                "▾"
            } else {
                "▸"
            }
        } else {
            "•"
        };
        let mut label = row.name.clone();
        if row.loading {
            label.push_str(" …");
        }

        // Git tint (docs/38 FILE-6): color the name by working-tree status, and
        // badge changed files with a letter on the right.
        let git = app.file_git_status.get(&row.path).copied();
        let base_fg = if row.is_dir { t.subtext1 } else { t.subtext0 };
        let git_fg = git.and_then(|s| git_color(s, t));
        let mut style = Style::new().fg(git_fg.unwrap_or(base_fg));
        if row.is_dir {
            style = style.bold();
        }
        if hovered || selected {
            style = style.fg(t.accent);
        }
        // A folder's chevron keeps the folder's own styling; a file's dot sits
        // one step quieter than its name, so a long list still reads as names
        // first and the dots recede into a column. It still picks up the git
        // tint, so a changed file reads as one coloured row rather than a
        // coloured name beside a grey dot.
        let marker_style = if row.is_dir {
            style
        } else {
            Style::new().fg(if hovered || selected {
                t.accent
            } else {
                git_fg.unwrap_or(t.overlay1)
            })
        };
        let mut spans = vec![
            Span::styled(format!("{indent}{glyph} "), marker_style),
            Span::styled(label, style),
        ];
        if let Some(badge) = git.map(|s| s.badge()).filter(|b| !b.is_empty()) {
            spans.push(Span::styled(
                format!(" {badge}"),
                Style::new().fg(git_fg.unwrap_or(t.overlay1)),
            ));
        }
        if selected {
            f.buffer_mut().set_style(rect, Style::new().bg(t.surface1));
        }
        line_at(f, y, Line::from(spans));
        app.file_tree_rects.push((i, rect));
    }
}

fn draw_diff_list(
    f: &mut RenderTarget,
    area: Rect,
    list_top: u16,
    cap: usize,
    app: &mut App,
    t: &Theme,
    line_at: &impl Fn(&mut RenderTarget, u16, Line),
) {
    if !app.diff_snapshot_matches_active_workspace() {
        line_at(
            f,
            list_top,
            Line::from(Span::styled("loading…", Style::new().fg(t.overlay1))),
        );
        return;
    }
    if let Some(error) = app.diff.error.as_deref() {
        line_at(
            f,
            list_top,
            Line::from(Span::styled(
                clip(error, area.width.saturating_sub(3)),
                Style::new().fg(t.coral),
            )),
        );
        return;
    }
    if app.diff.rows.is_empty() {
        line_at(
            f,
            list_top,
            Line::from(Span::styled(
                "working tree clean",
                Style::new().fg(t.overlay1),
            )),
        );
        return;
    }
    let max_scroll = app.diff.rows.len().saturating_sub(cap);
    app.diff.scroll = app.diff.scroll.min(max_scroll);
    app.diff.viewport = cap;
    let snapshot = app.diff.snapshot.as_ref().expect("rows require snapshot");
    let rows = &app.diff.rows;
    for row_index in app.diff.scroll..rows.len().min(app.diff.scroll.saturating_add(cap)) {
        let y = list_top + row_index.saturating_sub(app.diff.scroll) as u16;
        match &rows[row_index] {
            crate::diff::DiffListRow::Group(layer) => {
                let count = rows
                    .iter()
                    .skip(row_index + 1)
                    .take_while(|row| !matches!(row, crate::diff::DiffListRow::Group(_)))
                    .count();
                line_at(
                    f,
                    y,
                    Line::from(Span::styled(
                        format!("{}  {count}", layer.label().to_uppercase()),
                        Style::new().fg(t.overlay1).bold(),
                    )),
                );
            }
            crate::diff::DiffListRow::File(file_index) => {
                let Some(file) = snapshot.files.get(*file_index) else {
                    continue;
                };
                let selected = row_index == app.diff.cursor;
                let fg =
                    match file.status {
                        crate::diff::DiffFileStatus::Added
                        | crate::diff::DiffFileStatus::Untracked => t.mint,
                        crate::diff::DiffFileStatus::Deleted
                        | crate::diff::DiffFileStatus::Conflict => t.coral,
                        crate::diff::DiffFileStatus::Renamed
                        | crate::diff::DiffFileStatus::Copied => t.accent,
                        _ => t.amber,
                    };
                let path = if file.status == crate::diff::DiffFileStatus::Renamed {
                    match (&file.key.old_path, &file.key.new_path) {
                        (Some(old), Some(new)) => format!("{} → {}", old.display, new.display),
                        _ => file.key.display_path().to_string(),
                    }
                } else {
                    file.key.display_path().to_string()
                };
                let stats = match (file.additions, file.deletions) {
                    (Some(add), Some(del)) => format!(" +{add} -{del}"),
                    _ => String::new(),
                };
                let notes = diff_note_count(file.unresolved_notes);
                let marker = if file.modified_since_review() {
                    "↻"
                } else if file.viewed() {
                    "✓"
                } else {
                    " "
                };
                let style = if selected {
                    Style::new().fg(t.base).bg(t.accent).bold()
                } else {
                    Style::new().fg(t.subtext0)
                };
                let badge_style = if selected { style } else { Style::new().fg(fg) };
                line_at(
                    f,
                    y,
                    Line::from(vec![
                        Span::styled(format!("{marker} {} ", file.status.badge()), badge_style),
                        Span::styled(format!("{path}{stats}{notes}"), style),
                    ]),
                );
                app.diff_row_rects
                    .push((row_index, Rect::new(area.x, y, area.width, 1)));
            }
        }
    }
}

/// Draw a native file view (docs/38 FILE-3) into `area`, the pane's content
/// rect. O(visible rows): only the on-screen slice of `lines` is rendered. The
/// bottom row is a dim status footer.
pub(super) fn draw_file_view(
    f: &mut RenderTarget,
    area: Rect,
    v: &FileView,
    sel: Option<&crate::app::Selection>,
    mobile: bool,
    t: &Theme,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let show_footer = !mobile || v.search.is_some();
    let body = Rect::new(
        area.x,
        area.y,
        area.width,
        area.height.saturating_sub(u16::from(show_footer)),
    );
    let footer_y = area.bottom().saturating_sub(1);

    match &v.load {
        FileLoad::Loading => center(f, body, "loading…", t.overlay0),
        FileLoad::Binary(n) => center(f, body, &format!("binary file · {}", human(*n)), t.overlay1),
        FileLoad::TooLarge(n) => center(
            f,
            body,
            &format!(
                "too large to preview · {} (cap {})",
                human(*n),
                human(SIZE_CAP)
            ),
            t.overlay1,
        ),
        FileLoad::Error(e) => center(f, body, &format!("cannot open: {e}"), t.coral),
        FileLoad::Text(lines) => draw_text(f, body, v, lines, t),
    }

    // Mouse selection highlight (docs/38): overlay the selection background on
    // the selected cells, after the text so it tints whatever is under it. A
    // buffer post-pass keeps it independent of the text/search spans.
    if let Some(sel) = sel {
        // Line numbers are presentation-only and selection_text deliberately
        // excludes them, so do not tint the gutter as though it will be copied.
        let text_x = body.x + crate::files::gutter_width(v.line_count()) + 1;
        let buf = f.buffer_mut();
        for y in body.y..body.bottom() {
            for x in body.x..body.right() {
                if file_selection_contains(sel, x, y, text_x) {
                    if let Some(cell) = buf.cell_mut((x, y)) {
                        cell.set_bg(t.sel_bg);
                    }
                }
            }
        }
    }

    if !show_footer {
        return;
    }

    // Footer: path · lines · encoding, or the state.
    let name = v
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // A search overrides the footer with the query + hit position.
    let foot = if let Some(s) = &v.search {
        if s.editing {
            format!(" /{}", s.query)
        } else if s.matches.is_empty() {
            format!(" /{} · no matches", s.query)
        } else {
            format!(" /{} · {}/{}", s.query, s.current + 1, s.matches.len())
        }
    } else {
        match &v.load {
            FileLoad::Text(lines) => format!(" {name} · {} lines · UTF-8", lines.len()),
            FileLoad::Binary(_) => format!(" {name} · binary"),
            FileLoad::TooLarge(_) => format!(" {name} · too large"),
            FileLoad::Loading => format!(" {name} · loading…"),
            FileLoad::Error(_) => format!(" {name} · error"),
        }
    };
    let wrap_hint = if v.wrap { " wrap " } else { "" };
    let foot = clip(&foot, area.width.saturating_sub(wrap_hint.len() as u16));
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(foot, Style::new().fg(t.overlay0)))),
        Rect::new(area.x, footer_y, area.width, 1),
    );
    if v.wrap {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                wrap_hint,
                Style::new().fg(t.base).bg(t.overlay0),
            ))),
            Rect::new(area.right().saturating_sub(6), footer_y, 6, 1),
        );
    }
}

fn file_selection_contains(sel: &crate::app::Selection, x: u16, y: u16, text_x: u16) -> bool {
    x >= text_x && sel.contains(x, y)
}

fn draw_text(f: &mut RenderTarget, body: Rect, v: &FileView, lines: &[String], t: &Theme) {
    let rows = body.height as usize;
    // Shared with mouse-selection extraction so their columns agree.
    let gutter = crate::files::gutter_width(lines.len());
    let text_x = body.x + gutter + 1;
    let text_w = body.width.saturating_sub(gutter + 1);
    if text_w == 0 {
        return;
    }
    // The gutter is `marker + number + one space`, totalling `gutter + 1` — the
    // same width as before, so `text_x` and mouse-selection column mapping are
    // unchanged. The git change marker (docs/38 + docs/30) sits in column 0,
    // against the pane edge like an editor's change bar, rather than between the
    // number and the text where it split the two apart. A wrapped continuation
    // row keeps the marker (the line is still changed) but drops the number.
    let num_w = gutter.saturating_sub(1) as usize;
    let gutter_cell = |f: &mut RenderTarget, y: u16, num: Option<usize>, line: usize| {
        let s = match num {
            Some(n) => format!("{:>w$} ", n, w = num_w),
            // Continuation rows leave the number blank so a wrapped line reads as
            // one paragraph, not many numbered lines.
            None => " ".repeat(num_w + 1),
        };
        let (mark, mark_fg) = match v.change_at(line) {
            Some(ChangeKind::Added) => ("▎", t.green),
            Some(ChangeKind::Modified) => ("▎", t.amber),
            // Nothing survives to highlight, so flag the gap under this line.
            Some(ChangeKind::Removed) => ("▁", t.coral),
            None => (" ", t.overlay0),
        };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(mark, Style::new().fg(mark_fg)),
                Span::styled(s, Style::new().fg(t.overlay0)),
            ])),
            Rect::new(body.x, y, gutter + 1, 1),
        );
    };

    if v.wrap {
        // Soft-wrap: each file line occupies as many screen rows as it needs.
        // Scroll stays line-based (top row = file line `scroll`), so vertical
        // scroll, goto, and search reveal are unchanged.
        let mut y = body.y;
        let bottom = body.y + body.height;
        let mut i = v.scroll;
        while y < bottom && i < lines.len() {
            let line = &lines[i];
            for (si, range) in crate::files::wrap_ranges(line, text_w as usize)
                .into_iter()
                .enumerate()
            {
                if y >= bottom {
                    break;
                }
                gutter_cell(f, y, (si == 0).then_some(i + 1), i + 1);
                f.render_widget(
                    Paragraph::new(Span::styled(
                        crate::files::seg_text(line, range),
                        Style::new().fg(t.text),
                    )),
                    Rect::new(text_x, y, text_w, 1),
                );
                y += 1;
            }
            i += 1;
        }
        return;
    }

    // No-wrap: one file line per row, clipped, with horizontal scroll.
    for (i, line) in lines.iter().enumerate().skip(v.scroll).take(rows) {
        let y = body.y + (i - v.scroll) as u16;
        gutter_cell(f, y, Some(i + 1), i + 1);
        let line_ui = search_line(v, i, line, t);
        f.render_widget(
            Paragraph::new(line_ui).scroll((0, v.hscroll)),
            Rect::new(text_x, y, text_w, 1),
        );
    }
}

fn center(f: &mut RenderTarget, area: Rect, msg: &str, fg: ratatui::style::Color) {
    if area.height == 0 {
        return;
    }
    let y = area.y + area.height / 2;
    let msg = clip(msg, area.width);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(msg, Style::new().fg(fg))))
            .alignment(ratatui::layout::Alignment::Center),
        Rect::new(area.x, y, area.width, 1),
    );
}

/// Clip a string to `w` display columns (char count; ASCII-dominated source).
fn clip(s: &str, w: u16) -> String {
    s.chars().take(w as usize).collect()
}

fn human(n: u64) -> String {
    if n >= 1 << 20 {
        format!("{:.1} MB", n as f64 / (1 << 20) as f64)
    } else if n >= 1 << 10 {
        format!("{:.1} KB", n as f64 / (1 << 10) as f64)
    } else {
        format!("{n} B")
    }
}

/// Build a line's spans, highlighting any search matches on it (the current
/// match brighter). No match → one plain span.
fn search_line<'a>(v: &FileView, line_idx: usize, line: &'a str, t: &Theme) -> Line<'a> {
    let Some(s) = &v.search else {
        return Line::from(Span::styled(line, Style::new().fg(t.text)));
    };
    let hits: Vec<(usize, usize)> = s
        .matches
        .iter()
        .enumerate()
        .filter(|(_, (l, _))| *l == line_idx)
        .map(|(i, (_, c))| (i, *c))
        .collect();
    if hits.is_empty() || s.query.is_empty() {
        return Line::from(Span::styled(line, Style::new().fg(t.text)));
    }
    let qlen = s.query.chars().count();
    let mut spans: Vec<Span> = Vec::new();
    let mut cursor = 0usize; // char index
    let chars: Vec<char> = line.chars().collect();
    for (mi, col) in hits {
        if col > cursor {
            let seg: String = chars[cursor..col.min(chars.len())].iter().collect();
            spans.push(Span::styled(seg, Style::new().fg(t.text)));
        }
        let end = (col + qlen).min(chars.len());
        let seg: String = chars[col..end].iter().collect();
        let hl = if mi == s.current {
            Style::new().fg(t.base).bg(t.accent).bold()
        } else {
            Style::new().fg(t.base).bg(t.amber)
        };
        spans.push(Span::styled(seg, hl));
        cursor = end;
    }
    if cursor < chars.len() {
        let seg: String = chars[cursor..].iter().collect();
        spans.push(Span::styled(seg, Style::new().fg(t.text)));
    }
    Line::from(spans)
}

/// The tint color for a git working-tree status in the FILES dock (docs/38).
fn git_color(s: crate::git::local::FileStatus, t: &Theme) -> Option<ratatui::style::Color> {
    use crate::git::local::FileStatus::*;
    Some(match s {
        Modified | DirDirty => t.amber,
        Added | Untracked => t.green,
        Deleted => t.coral,
        Renamed => t.mint,
        Conflict => t.coral,
    })
}

/// The title line for a create/rename prompt (docs/38 FILE-6).
pub(super) fn file_prompt_title(p: &crate::app::FilePrompt) -> &'static str {
    use crate::app::FilePromptKind::*;
    match p.kind {
        NewFile => "New file",
        NewFolder => "New folder",
        Rename => "Rename",
    }
}

/// The delete-confirm modal: "Delete <name>?" with y / esc footer hints.
pub(super) fn draw_delete_confirm(
    f: &mut RenderTarget,
    area: Rect,
    path: &std::path::Path,
    heading: Option<&str>,
    hover: Option<(u16, u16)>,
    t: &Theme,
) -> (Option<Rect>, Option<Rect>) {
    use ratatui::layout::Alignment;
    use ratatui::widgets::{Block, Borders, Clear};
    // Dim backdrop.
    let buf = f.buffer_mut();
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            if let Some(c) = buf.cell_mut((x, y)) {
                c.set_bg(t.crust);
            }
        }
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let is_dir = path.is_dir();
    let w = area.width.saturating_sub(6).clamp(30, 60).min(area.width);
    let h = 6u16;
    let mx = area.x + (area.width.saturating_sub(w)) / 2;
    let my = area.y + (area.height.saturating_sub(h)) / 2;
    let modal = Rect::new(mx, my, w, h);
    f.render_widget(Clear, modal);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(t.coral).bg(t.surface0))
        .style(Style::new().bg(t.surface0));
    let inner = block.inner(modal);
    f.render_widget(block, modal);
    let what = if is_dir {
        "folder (and its contents)"
    } else {
        "file"
    };
    let head = heading
        .map(str::to_string)
        .unwrap_or_else(|| format!("Delete {what}?"));
    f.render_widget(
        Paragraph::new(Span::styled(head, Style::new().fg(t.text).bold()))
            .alignment(Alignment::Center),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    f.render_widget(
        Paragraph::new(Span::styled(name, Style::new().fg(t.coral).bold()))
            .alignment(Alignment::Center),
        Rect::new(inner.x, inner.y + 2, inner.width, 1),
    );
    // Footer: y delete · esc cancel (clickable rects).
    let footer_y = inner.bottom().saturating_sub(1);
    let del = " y delete ";
    let cancel = " esc cancel ";
    let dw = del.chars().count() as u16;
    let del_rect = Rect::new(inner.x, footer_y, dw.min(inner.width), 1);
    let cancel_x = (inner.x + dw + 1).min(inner.right());
    let cancel_rect = Rect::new(
        cancel_x,
        footer_y,
        (cancel.chars().count() as u16).min(inner.right().saturating_sub(cancel_x)),
        1,
    );
    let over = |r: Rect| hover.is_some_and(|(c, hr)| c >= r.x && c < r.right() && hr == r.y);
    let hl = |on: bool, fg| {
        if on {
            Style::new().fg(t.crust).bg(fg).bold()
        } else {
            Style::new().fg(fg).bold()
        }
    };
    f.render_widget(
        Paragraph::new(Span::styled(del, hl(over(del_rect), t.coral))),
        del_rect,
    );
    f.render_widget(
        Paragraph::new(Span::styled(cancel, hl(over(cancel_rect), t.overlay1))),
        cancel_rect,
    );
    (Some(del_rect), Some(cancel_rect))
}

#[cfg(test)]
mod tests {
    use super::{diff_note_count, file_selection_contains};
    use crate::app::Selection;
    use crate::ids::PaneId;
    use ratatui::layout::Rect;

    #[test]
    fn diff_note_count_uses_singular_and_plural_labels() {
        assert_eq!(diff_note_count(0), "");
        assert_eq!(diff_note_count(1), "  1 note");
        assert_eq!(diff_note_count(2), "  2 notes");
    }

    #[test]
    fn file_selection_highlight_excludes_the_line_number_gutter() {
        let selection = Selection {
            pane: PaneId(1),
            content: Rect::new(2, 1, 20, 4),
            anchor: (9, 1),
            cursor: (12, 3),
            retained: None,
            scrolled: false,
            dragging: true,
        };
        let text_x = 7;

        assert!(!file_selection_contains(&selection, 2, 2, text_x));
        assert!(!file_selection_contains(&selection, 6, 2, text_x));
        assert!(file_selection_contains(&selection, 7, 2, text_x));
        assert!(file_selection_contains(&selection, 12, 3, text_x));
    }
}
