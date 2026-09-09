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
use crate::files::{highlight, FileLoad, FileView, SIZE_CAP};
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
    let cx = area.x + 2;
    let cw = area.width.saturating_sub(3);
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
                let stats = diff_list_stats(file.additions, file.deletions, t);
                let stats_width = stats.as_ref().map_or(0, Line::width) as u16;
                // Keep enough room for the review marker, status badge, and a
                // useful path fragment. Very narrow docks omit counts instead
                // of allowing the right column to overwrite the file label.
                let show_stats = stats_width > 0 && cw >= stats_width.saturating_add(8);
                let label_width = if show_stats {
                    cw.saturating_sub(stats_width).saturating_sub(1)
                } else {
                    cw
                };
                f.buffer_mut().set_line(
                    cx,
                    y,
                    &Line::from(vec![
                        Span::styled(format!("{marker} {} ", file.status.badge()), badge_style),
                        Span::styled(format!("{path}{notes}"), style),
                    ]),
                    label_width,
                );
                if show_stats {
                    f.buffer_mut().set_line(
                        cx + cw - stats_width,
                        y,
                        stats.as_ref().expect("visible stats require a line"),
                        stats_width,
                    );
                }
                app.diff_row_rects
                    .push((row_index, Rect::new(area.x, y, area.width, 1)));
            }
        }
    }
}

fn diff_list_stats(
    additions: Option<u32>,
    deletions: Option<u32>,
    t: &Theme,
) -> Option<Line<'static>> {
    let (additions, deletions) = additions.zip(deletions)?;
    Some(Line::from(vec![
        Span::styled(format!("+{additions}"), Style::new().fg(t.mint)),
        Span::styled(" ", Style::new().fg(t.subtext0)),
        Span::styled(format!("-{deletions}"), Style::new().fg(t.coral)),
    ]))
}

/// Draw a native file view (docs/38 FILE-3) into `area`, the pane's content
/// rect. O(visible rows × visible width): only the on-screen slice of `lines`
/// is rendered, and each on-screen logical line is tokenized only up to its
/// visible char window (cached per generation), so a very long logical line
/// costs its viewport slice, not the file. The bottom row is a dim status
/// footer.
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
    // The highlight language is a pure function of the path: one lookup per
    // frame, then O(visible rows × visible width) windowed tokenization below
    // — never O(file). A multi-megabyte single logical line costs a viewport,
    // not the file: tokens come from the bounded generation-checked cache and
    // cover only the visible char window.
    let lang = highlight::language_for_path(&v.path);
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
        // scroll, goto, and search reveal are unchanged. Each segment is
        // syntax-highlighted from the line's windowed tokens, with search
        // matches overlaid — the same spans the no-wrap path builds.
        //
        // Bounded per frame: `wrap_ranges_limited` computes only the rows we
        // will actually draw, and `highlight_window` tokenizes only up to the
        // last visible segment end (cached per generation).
        let mut y = body.y;
        let bottom = body.y + body.height;
        let mut i = v.scroll;
        while y < bottom && i < lines.len() {
            let line = &lines[i];
            let remaining = (bottom - y) as usize;
            let ranges = crate::files::wrap_ranges_limited(line, text_w as usize, remaining);
            let needed_end = ranges.iter().map(|&(_, e)| e).max().unwrap_or(0);
            let open = v.string_states.get(i).copied().flatten();
            let tokens = v.highlight_window(i, line, lang, open, needed_end);
            let hits = search_hits_for_line(v, i);
            for (si, range) in ranges.into_iter().enumerate() {
                if y >= bottom {
                    break;
                }
                gutter_cell(f, y, (si == 0).then_some(i + 1), i + 1);
                let spans = spans_in_range(line, &tokens, range, &hits, t);
                f.render_widget(
                    Paragraph::new(Line::from(spans)),
                    Rect::new(text_x, y, text_w, 1),
                );
                y += 1;
            }
            i += 1;
        }
        return;
    }

    // No-wrap: one file line per row. Only the visible `[hscroll,
    // hscroll+text_w)` char window is tokenized and spanned — never the whole
    // multi-megabyte line — and the paragraph needs no scroll offset.
    for (i, line) in lines.iter().enumerate().skip(v.scroll).take(rows) {
        let y = body.y + (i - v.scroll) as u16;
        gutter_cell(f, y, Some(i + 1), i + 1);
        let open = v.string_states.get(i).copied().flatten();
        let hstart = v.hscroll as usize;
        let needed_end = hstart.saturating_add(text_w as usize);
        let tokens = v.highlight_window(i, line, lang, open, needed_end);
        let hits = search_hits_for_line(v, i);
        let spans = spans_in_range(line, &tokens, (hstart, needed_end), &hits, t);
        f.render_widget(
            Paragraph::new(Line::from(spans)),
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

/// Base text style for one highlight role, resolved through the active theme
/// so a theme switch recolors the viewer without reparsing any file.
fn highlight_style(kind: highlight::Kind, t: &Theme) -> Style {
    match kind {
        highlight::Kind::Normal => Style::new().fg(t.text),
        highlight::Kind::Keyword => Style::new().fg(t.accent),
        highlight::Kind::String => Style::new().fg(t.green),
        highlight::Kind::Comment => Style::new().fg(t.overlay1).italic(),
        highlight::Kind::Number => Style::new().fg(t.amber),
        highlight::Kind::Function => Style::new().fg(t.mint),
        highlight::Kind::Type => Style::new().fg(t.subtext1),
    }
}

/// Search hits on one file line as `(start_col, end_col, is_current)` in
/// original-line character columns. `None`/editing/empty queries yield no
/// hits, so plain syntax spans render alone.
///
/// Matches are stored in document order, so the per-line slice is found with
/// two binary searches — O(log matches + hits), not O(matches) per visible
/// row.
fn search_hits_for_line(v: &FileView, line_idx: usize) -> Vec<(usize, usize, bool)> {
    let Some(search) = &v.search else {
        return Vec::new();
    };
    if search.editing || search.query.is_empty() {
        return Vec::new();
    }
    let matches = &search.matches;
    let lo = matches.partition_point(|(l, _, _)| *l < line_idx);
    let hi = matches.partition_point(|(l, _, _)| *l <= line_idx);
    matches[lo..hi]
        .iter()
        .enumerate()
        .map(|(k, (_, start, end))| (*start, *end, lo + k == search.current))
        .collect()
}

/// Build spans for the char range of one line: syntax tokens clipped to the
/// range, split at search-match boundaries so the current match stays brighter
/// than the rest. Adjacent pieces with the same style merge back into one span.
///
/// Only the visible `range` is materialized: the segment text is collected via
/// `skip`/`take` (no whole-line `Vec<char>`), so a 5 MiB line costs its
/// viewport slice, not the file.
fn spans_in_range(
    line: &str,
    tokens: &[highlight::Token],
    range: (usize, usize),
    hits: &[(usize, usize, bool)],
    t: &Theme,
) -> Vec<Span<'static>> {
    let (seg_start, seg_end) = range;
    if seg_start >= seg_end {
        return Vec::new();
    }
    let seg_len = seg_end - seg_start;
    // Visible slice only.
    let seg_chars: Vec<char> = line.chars().skip(seg_start).take(seg_len).collect();
    let text_of = |start: usize, end: usize| -> String {
        let (lo, hi) = (
            start.saturating_sub(seg_start),
            end.saturating_sub(seg_start),
        );
        let (lo, hi) = (lo.min(seg_chars.len()), hi.min(seg_chars.len()));
        if lo >= hi {
            String::new()
        } else {
            seg_chars[lo..hi].iter().collect()
        }
    };
    let mut intervals: Vec<(usize, usize, bool)> = hits
        .iter()
        .map(|(start, end, current)| (*start, *end, *current))
        .map(|(start, end, current)| (start.max(seg_start), end.min(seg_end), current))
        .filter(|(start, end, _)| start < end)
        .collect();
    intervals.sort();
    intervals.dedup();
    let mut pieces: Vec<(String, Style)> = Vec::new();
    let mut push = |text: String, style: Style| {
        if text.is_empty() {
            return;
        }
        if let Some(last) = pieces.last_mut().filter(|(_, prev)| *prev == style) {
            last.0.push_str(&text);
        } else {
            pieces.push((text, style));
        }
    };
    for token in tokens {
        let token_start = token.start.max(seg_start);
        let token_end = token.end.min(seg_end);
        if token_start >= token_end {
            continue;
        }
        let base = highlight_style(token.kind, t);
        let mut cursor = token_start;
        for (hit_start, hit_end, current) in intervals
            .iter()
            .filter(|(start, end, _)| *end > token_start && *start < token_end)
        {
            let hit_start = (*hit_start).max(token_start);
            let hit_end = (*hit_end).min(token_end);
            if hit_start > cursor {
                push(text_of(cursor, hit_start), base);
            }
            let highlight = if *current {
                Style::new().fg(t.base).bg(t.accent).bold()
            } else {
                Style::new().fg(t.base).bg(t.amber)
            };
            push(text_of(hit_start, hit_end), highlight);
            cursor = hit_end;
        }
        if cursor < token_end {
            push(text_of(cursor, token_end), base);
        }
    }
    pieces
        .into_iter()
        .map(|(text, style)| Span::styled(text, style))
        .collect()
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
    use super::{diff_list_stats, diff_note_count, file_selection_contains};
    use crate::app::Selection;
    use crate::ids::PaneId;
    use crate::ui::{theme::Theme, RenderTarget};
    use ratatui::{buffer::Buffer, layout::Rect};

    #[test]
    fn diff_note_count_uses_singular_and_plural_labels() {
        assert_eq!(diff_note_count(0), "");
        assert_eq!(diff_note_count(1), "  1 note");
        assert_eq!(diff_note_count(2), "  2 notes");
    }

    #[test]
    fn diff_stats_are_colored_and_right_aligned() {
        let theme = Theme::quattro_rally();
        let area = Rect::new(0, 0, 24, 1);
        let stats = diff_list_stats(Some(114), Some(25), &theme).unwrap();
        let width = stats.width() as u16;
        let mut buffer = Buffer::empty(area);
        {
            let mut target = RenderTarget::new(&mut buffer, area);
            target
                .buffer_mut()
                .set_line(area.right() - width, 0, &stats, width);
        }

        let screen: String = (0..area.width).map(|x| buffer[(x, 0)].symbol()).collect();
        assert!(screen.ends_with("+114 -25"));
        assert_eq!(buffer[(area.right() - width, 0)].fg, theme.mint);
        assert_eq!(buffer[(area.right() - 3, 0)].fg, theme.coral);
        assert_ne!(buffer[(area.right() - width, 0)].bg, theme.accent);
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

    /// Syntax spans keep the line's text intact while coloring each role
    /// through the theme: keywords use the accent, strings the idle green,
    /// comments the dim overlay, numbers amber, calls mint, types subtext.
    #[test]
    fn rust_viewer_spans_color_each_role_through_the_theme() {
        use crate::files::highlight;

        let theme = Theme::quattro_rally();
        let line = "fn load(path: &Path) -> usize { // open";
        let tokens = highlight::tokenize_continued(line, highlight::Language::Rust, None);
        let width = line.chars().count();
        let spans = super::spans_in_range(line, &tokens, (0, width), &[], &theme);
        let text: String = spans.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(text, line, "spans must cover the line exactly");

        let style_of = |needle: &str| {
            spans
                .iter()
                .find(|span| span.content == needle)
                .map(|span| span.style)
        };
        assert_eq!(
            style_of("fn").map(|style| style.fg),
            Some(Some(theme.accent)),
            "keywords use the accent"
        );
        assert_eq!(
            style_of("load").map(|style| style.fg),
            Some(Some(theme.mint)),
            "calls use mint"
        );
        assert_eq!(
            style_of("Path").map(|style| style.fg),
            Some(Some(theme.subtext1)),
            "types use subtext"
        );
        assert!(
            spans
                .iter()
                .find(|span| span.content.contains("// open"))
                .is_some_and(|span| span.style.fg == Some(theme.overlay1)),
            "the trailing comment is dim"
        );
    }

    /// Search matches overlay the syntax color with the same backgrounds the
    /// viewer used before highlighting, and wrapped segments clip both layers
    /// to their own char range.
    #[test]
    fn viewer_search_overlays_syntax_and_clips_to_wrap_segments() {
        use crate::files::highlight;

        let theme = Theme::quattro_rally();
        let line = "let loaded = load(path); // load";
        let tokens = highlight::tokenize_continued(line, highlight::Language::Rust, None);
        let width = line.chars().count();
        // `load` occurs at columns 4 ("loaded" prefix) and 13; highlight the
        // standalone call as the current match.
        let spans = super::spans_in_range(line, &tokens, (0, width), &[(13, 17, true)], &theme);
        let current = spans
            .iter()
            .find(|span| span.content == "load")
            .expect("current match is its own span");
        assert_eq!(current.style.bg, Some(theme.accent));
        assert_eq!(current.style.fg, Some(theme.base));

        // A wrapped segment sees only its own slice of both layers.
        let segment = super::spans_in_range(line, &tokens, (0, 10), &[(13, 17, true)], &theme);
        let text: String = segment.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(text, "let loaded");
        assert!(
            segment.iter().all(|span| span.style.bg.is_none()),
            "the match outside the segment must not leak in"
        );
    }

    /// Near-limit single-line JavaScript: windowed spans cover only the
    /// viewport slice, stay coalesced, clip search to the window, and recolor
    /// across themes without re-tokenizing.
    #[test]
    fn huge_single_line_js_spans_stay_windowed_across_themes() {
        use crate::files::{highlight, FileLoad, FileView};
        use std::path::PathBuf;

        let unit = r#"const v0123456789="abcdefghij0123456789";"#;
        let reps = (2_000_000 / unit.len()).max(1);
        let huge: String = unit.repeat(reps);
        assert!(huge.len() > 1_000_000);

        let mut v = FileView::new(PathBuf::from("bundle.min.js"));
        let lang = highlight::language_for_path(&v.path);
        let lines = vec![huge.clone()];
        let states = highlight::continuation_states(&lines, lang);
        v.apply_prepared(FileLoad::Text(lines), states);

        // No-wrap window: hscroll + width.
        v.wrap = false;
        v.hscroll = 200;
        let text_w = 80usize;
        let needed = 200 + text_w;
        let tokens = v.highlight_window(0, &huge, lang, None, needed);
        assert!(tokens.iter().all(|t| t.start < needed));
        let hits = super::search_hits_for_line(&v, 0);
        assert!(hits.is_empty());
        let spans =
            super::spans_in_range(&huge, &tokens, (200, needed), &[], &Theme::quattro_rally());
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        let expected: String = huge.chars().skip(200).take(text_w).collect();
        assert_eq!(text, expected);
        assert!(
            spans.len() < text_w,
            "coalesced spans stay far below one-per-char (got {})",
            spans.len()
        );

        // Wrap window: first viewport rows only.
        v.wrap = true;
        let ranges = crate::files::wrap_ranges_limited(&huge, text_w, 40);
        assert_eq!(ranges.len(), 40);
        let needed = ranges.iter().map(|&(_, e)| e).max().unwrap();
        let tokens = v.highlight_window(0, &huge, lang, None, needed);
        let first = ranges[0];
        let spans = super::spans_in_range(&huge, &tokens, first, &[], &Theme::quattro_rally());
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        let expected: String = huge.chars().skip(first.0).take(first.1 - first.0).collect();
        assert_eq!(text, expected);

        // A match outside the window must not leak into it.
        let far_hit = vec![(needed + 1000, needed + 1004, true)];
        let spans = super::spans_in_range(&huge, &tokens, first, &far_hit, &Theme::quattro_rally());
        assert!(spans.iter().all(|s| s.style.bg.is_none()));

        // Theme change recolors the same tokens without re-tokenizing.
        let other = Theme::dracula();
        let a = super::spans_in_range(&huge, &tokens, first, &[], &Theme::quattro_rally());
        let b = super::spans_in_range(&huge, &tokens, first, &[], &other);
        let ta: String = a.iter().map(|s| s.content.as_ref()).collect();
        let tb: String = b.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(ta, tb, "same text across themes");
        assert_ne!(
            a.iter().map(|s| s.style).collect::<Vec<_>>(),
            b.iter().map(|s| s.style).collect::<Vec<_>>(),
            "styles resolve through the theme"
        );
        assert_eq!(v.token_cache_len(), 1, "one cached window reused");
    }

    /// End-to-end draw of a huge single-line file completes in both modes.
    #[test]
    fn huge_file_view_draws_in_both_wrap_modes() {
        use crate::files::{FileLoad, FileView};
        use std::path::PathBuf;

        let huge = "x".repeat(500_000);
        let mut v = FileView::new(PathBuf::from("data.json"));
        v.apply_prepared(FileLoad::Text(vec![huge.clone()]), vec![None]);
        let area = Rect::new(0, 0, 100, 24);
        for wrap in [true, false] {
            v.wrap = wrap;
            let mut buf = Buffer::empty(area);
            {
                let mut target = RenderTarget::new(&mut buf, area);
                super::draw_file_view(&mut target, area, &v, None, false, &Theme::quattro_rally());
            }
            let screen: String = (0..area.height)
                .map(|y| {
                    (0..area.width)
                        .map(|x| buf[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                screen.contains('x'),
                "the huge line's viewport slice drew (wrap={wrap})"
            );
        }
    }
}
