//! Native DIFF renderer (docs/88). It draws only the visible cached row slice.

use std::collections::HashMap;

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::diff::{
    DiffAgentPicker, DiffColorMode, DiffLayoutPreference, DiffLine, DiffLineKind, DiffLoad,
    DiffMarkerStyle, DiffSide, DiffState, DiffView, NoteState,
};
use crate::ui::theme::Theme;
use crate::ui::RenderTarget;

const SPLIT_MIN_WIDTH: u16 = 96;

pub(super) struct DiffRenderContext<'a> {
    pub state: &'a DiffState,
    pub picker: Option<&'a DiffAgentPicker>,
    pub marker_style: DiffMarkerStyle,
    pub color_mode: DiffColorMode,
    pub mobile: bool,
    pub source_hits: &'a mut Vec<(crate::ids::PaneId, usize, DiffSide, Rect)>,
    pub note_hits: &'a mut Vec<(crate::ids::PaneId, String, Rect)>,
}

struct DiffInteractionHits<'a> {
    pane: crate::ids::PaneId,
    interactive: bool,
    source: &'a mut Vec<(crate::ids::PaneId, usize, DiffSide, Rect)>,
    notes: &'a mut Vec<(crate::ids::PaneId, String, Rect)>,
}

struct DiffNotes<'a> {
    view: &'a DiffView,
    state: &'a DiffState,
}

#[derive(Clone, Copy)]
struct DiffRenderOptions {
    marker_style: DiffMarkerStyle,
    color_mode: DiffColorMode,
}

pub(super) fn draw_diff_view(
    f: &mut RenderTarget,
    area: Rect,
    id: crate::ids::PaneId,
    view: &DiffView,
    context: DiffRenderContext<'_>,
    t: &Theme,
) {
    let DiffRenderContext {
        state,
        picker,
        marker_style,
        color_mode,
        mobile,
        source_hits,
        note_hits,
    } = context;
    let options = DiffRenderOptions {
        marker_style,
        color_mode,
    };
    let (addition_color, deletion_color) = change_colors(color_mode, t);
    if area.width == 0 || area.height == 0 {
        return;
    }
    let header = Rect::new(area.x, area.y, area.width, 1);
    let show_footer =
        !mobile || view.note_draft.is_some() || view.note_selecting || view.search_editing;
    let footer = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
    let body = Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        area.height.saturating_sub(1 + u16::from(show_footer)),
    );
    let effective = if view.wrap {
        DiffLayoutPreference::Stack
    } else {
        effective_layout(view.preference, area.width)
    };
    let narrow_fallback = view.preference == DiffLayoutPreference::Split
        && effective == DiffLayoutPreference::Stack
        && !view.wrap;
    let layout = if view.wrap && view.preference != DiffLayoutPreference::Stack {
        match view.preference {
            DiffLayoutPreference::Auto => "AUTO → STACK (wrap)",
            DiffLayoutPreference::Split => "SPLIT → STACK (wrap)",
            DiffLayoutPreference::Stack => unreachable!(),
        }
    } else if narrow_fallback {
        "SPLIT → STACK (narrow)"
    } else {
        effective.as_str()
    };
    let source_interactive =
        view.note_draft.is_none() && !picker.is_some_and(|active_picker| active_picker.view == id);
    let mut hits = DiffInteractionHits {
        pane: id,
        interactive: source_interactive,
        source: source_hits,
        notes: note_hits,
    };
    let position = state.snapshot.as_ref().and_then(|snapshot| {
        snapshot
            .files
            .iter()
            .position(|file| file.key == view.key)
            .map(|index| format!("{}/{}", index + 1, snapshot.files.len()))
    });
    let mut metadata = vec![Span::styled(
        view.key.layer.label().to_uppercase(),
        Style::new().fg(t.overlay1),
    )];
    if let DiffLoad::Ready(diff) = &view.load {
        metadata.push(Span::styled("  ", Style::new().fg(t.overlay1)));
        metadata.push(Span::styled(
            format!("+{}", diff.additions),
            Style::new().fg(addition_color),
        ));
        metadata.push(Span::styled(" ", Style::new().fg(t.overlay1)));
        metadata.push(Span::styled(
            format!("-{}", diff.deletions),
            Style::new().fg(deletion_color),
        ));
    }
    if let Some(position) = position {
        metadata.push(Span::styled(
            format!("  {position}"),
            Style::new().fg(t.overlay1),
        ));
    }
    metadata.push(Span::styled(
        format!("  {}", layout.to_uppercase()),
        Style::new().fg(t.overlay1),
    ));
    draw_diff_header(f, header, view.key.display_path(), Line::from(metadata), t);
    match &view.load {
        DiffLoad::Loading => center(f, body, "loading diff…", t.overlay1),
        DiffLoad::Error(error) => center(f, body, &format!("cannot load diff: {error}"), t.coral),
        DiffLoad::Conflict(summary) => center(f, body, summary, t.amber),
        DiffLoad::Ready(diff) if diff.binary => {
            center(f, body, "binary content is not rendered", t.overlay1)
        }
        DiffLoad::Ready(diff) if diff.hunks.is_empty() => {
            center(f, body, "no textual changes", t.overlay1)
        }
        DiffLoad::Ready(_) if effective == DiffLayoutPreference::Split => {
            draw_split(f, body, view, state, options, &mut hits, t)
        }
        DiffLoad::Ready(_) => draw_stack(f, body, view, state, options, &mut hits, t),
    }
    let truncated = matches!(&view.load, DiffLoad::Ready(diff) if diff.truncated);
    let note_count = state
        .notes
        .iter()
        .filter(|note| note.anchor.diff_key == view.key && note.state != NoteState::Resolved)
        .count();
    let viewed = state
        .snapshot
        .as_ref()
        .map(|snapshot| snapshot.files.iter().filter(|file| file.viewed()).count())
        .unwrap_or(0);
    let total = state
        .snapshot
        .as_ref()
        .map(|snapshot| snapshot.files.len())
        .unwrap_or(0);
    let mut hint = if view.note_draft.is_some() {
        " NOTE  inline editor open · save locally, then [a] sends to an agent".to_string()
    } else if view.note_selecting {
        " NOTE  select source · click a row or move with j/k · Enter writes · Esc cancels"
            .to_string()
    } else if view.search_editing {
        format!(" SEARCH> {}", view.search.as_deref().unwrap_or_default())
    } else {
        format!(
            " [s] layout  [/] search  [n] note  [a] send  [m] viewed  {viewed}/{total} · {note_count} notes"
        )
    };
    if truncated {
        hint.push_str("  TRUNCATED");
    }
    if show_footer {
        f.buffer_mut().set_line(
            footer.x,
            footer.y,
            &Line::from(Span::styled(
                clip(&hint, footer.width),
                Style::new().fg(if truncated { t.coral } else { t.overlay0 }),
            )),
            footer.width,
        );
    }
    if let Some(picker) = picker.filter(|picker| picker.view == id) {
        draw_agent_picker(f, body, picker, t);
    }
}

fn draw_diff_header(f: &mut RenderTarget, area: Rect, path: &str, metadata: Line<'_>, t: &Theme) {
    let metadata_width = metadata
        .spans
        .iter()
        .map(|span| super::display_width(&span.content))
        .sum::<usize>() as u16;
    let metadata_width = metadata_width.min(area.width);
    let metadata_x = area.right().saturating_sub(metadata_width);
    let path_width = metadata_x.saturating_sub(area.x).saturating_sub(1);

    if path_width > 0 {
        let path = super::truncate(path, path_width.saturating_sub(1) as usize);
        f.render_widget(
            Paragraph::new(Span::styled(
                format!(" {path}"),
                Style::new().fg(t.text).bold(),
            )),
            Rect::new(area.x, area.y, path_width, 1),
        );
    }
    if metadata_width > 0 {
        f.render_widget(
            Paragraph::new(metadata),
            Rect::new(metadata_x, area.y, metadata_width, 1),
        );
    }
}

fn effective_layout(preference: DiffLayoutPreference, width: u16) -> DiffLayoutPreference {
    match preference {
        DiffLayoutPreference::Stack => DiffLayoutPreference::Stack,
        DiffLayoutPreference::Split if width < SPLIT_MIN_WIDTH => DiffLayoutPreference::Stack,
        DiffLayoutPreference::Split => DiffLayoutPreference::Split,
        DiffLayoutPreference::Auto if width >= SPLIT_MIN_WIDTH => DiffLayoutPreference::Split,
        DiffLayoutPreference::Auto => DiffLayoutPreference::Stack,
    }
}

fn draw_stack(
    f: &mut RenderTarget,
    area: Rect,
    view: &DiffView,
    state: &DiffState,
    options: DiffRenderOptions,
    hits: &mut DiffInteractionHits<'_>,
    t: &Theme,
) {
    let notes = DiffNotes { view, state };
    let start = view.scroll.min(view.stack_rows.len().saturating_sub(1));
    let mut y = area.y;
    for (index, line) in view.stack_rows.iter().enumerate().skip(start) {
        if y >= area.bottom() {
            break;
        }
        let selected = index == view.selected || line_in_note_selection(view, line);
        let old = line.old_line.map_or(String::new(), |n| n.to_string());
        let new = line.new_line.map_or(String::new(), |n| n.to_string());
        let (bar, symbol) = gutter_markers(line.kind, options.marker_style);
        let numbers = if view.show_line_numbers {
            format!("{old:>5} {new:>5} ")
        } else {
            String::new()
        };
        let style = line_style(line.kind, selected, options.color_mode, t);
        let gutter_width = bar.chars().count() + numbers.chars().count() + symbol.chars().count();
        let text_w = area.width.saturating_sub(gutter_width as u16);
        if view.wrap {
            let fragments = wrapped_text(&line.text, text_w.max(1));
            for (fragment_index, text) in fragments.into_iter().enumerate() {
                if y >= area.bottom() {
                    break;
                }
                let (visible_bar, visible_numbers, visible_symbol) = if fragment_index == 0 {
                    (bar.clone(), numbers.clone(), symbol.clone())
                } else {
                    (
                        bar.clone(),
                        " ".repeat(numbers.chars().count()),
                        " ".repeat(symbol.chars().count()),
                    )
                };
                fill_bg(f, Rect::new(area.x, y, area.width, 1), style.bg);
                f.buffer_mut().set_line(
                    area.x,
                    y,
                    &Line::from(vec![
                        Span::styled(
                            visible_bar,
                            Style::new().fg(style.marker).bg(style.bg).bold(),
                        ),
                        Span::styled(visible_numbers, Style::new().fg(t.overlay1).bg(style.bg)),
                        Span::styled(
                            visible_symbol,
                            Style::new().fg(style.marker).bg(style.bg).bold(),
                        ),
                        Span::styled(text, Style::new().fg(style.text).bg(style.bg)),
                    ]),
                    area.width,
                );
                if hits.interactive {
                    if let Some((side, _)) = line_anchor(line) {
                        hits.source.push((
                            hits.pane,
                            index,
                            side,
                            Rect::new(area.x, y, area.width, 1),
                        ));
                    }
                }
                y = y.saturating_add(1);
            }
        } else {
            let text = horizontal_text(&line.text, view.horizontal, text_w);
            fill_bg(f, Rect::new(area.x, y, area.width, 1), style.bg);
            f.buffer_mut().set_line(
                area.x,
                y,
                &Line::from(vec![
                    Span::styled(bar, Style::new().fg(style.marker).bg(style.bg).bold()),
                    Span::styled(numbers, Style::new().fg(t.overlay1).bg(style.bg)),
                    Span::styled(symbol, Style::new().fg(style.marker).bg(style.bg).bold()),
                    Span::styled(text, Style::new().fg(style.text).bg(style.bg)),
                ]),
                area.width,
            );
            if hits.interactive {
                if let Some((side, _)) = line_anchor(line) {
                    hits.source
                        .push((hits.pane, index, side, Rect::new(area.x, y, area.width, 1)));
                }
            }
            y = y.saturating_add(1);
        }
        y = draw_notes_for_anchor(f, area, y, line_anchor(line), &notes, hits, t);
        if index == view.selected && view.note_draft.is_some() {
            y = draw_note_composer(f, area, y, view, t);
        }
    }
}

fn draw_split(
    f: &mut RenderTarget,
    area: Rect,
    view: &DiffView,
    state: &DiffState,
    options: DiffRenderOptions,
    hits: &mut DiffInteractionHits<'_>,
    t: &Theme,
) {
    let notes = DiffNotes { view, state };
    let mut stack_indices = HashMap::new();
    for (index, line) in view.stack_rows.iter().enumerate() {
        if let Some(number) = line.old_line {
            stack_indices.insert((DiffSide::Old, number), index);
        }
        if let Some(number) = line.new_line {
            stack_indices.insert((DiffSide::New, number), index);
        }
    }
    let selected_anchor = view.stack_rows.get(view.selected).and_then(line_anchor);
    let scroll_anchor = view.stack_rows.get(view.scroll).and_then(line_anchor);
    let start = scroll_anchor
        .and_then(|anchor| {
            view.split_rows.iter().position(|row| {
                row.old.as_ref().and_then(line_anchor) == Some(anchor)
                    || row.new.as_ref().and_then(line_anchor) == Some(anchor)
            })
        })
        .unwrap_or(0);
    let half = area.width.saturating_sub(1) / 2;
    let mut y = area.y;
    for row in view.split_rows.iter().skip(start) {
        if y >= area.bottom() {
            break;
        }
        let selected = selected_anchor.is_some_and(|anchor| {
            row.old.as_ref().and_then(line_anchor) == Some(anchor)
                || row.new.as_ref().and_then(line_anchor) == Some(anchor)
        });
        let old_selected = if view.note_selecting {
            row.old.as_ref().is_some_and(|line| {
                line.old_line
                    .is_some_and(|number| anchor_in_note_selection(view, DiffSide::Old, number))
            })
        } else {
            selected
        };
        let new_selected = if view.note_selecting {
            row.new.as_ref().is_some_and(|line| {
                line.new_line
                    .is_some_and(|number| anchor_in_note_selection(view, DiffSide::New, number))
            })
        } else {
            selected
        };
        let old_rect = Rect::new(area.x, y, half, 1);
        let new_rect = Rect::new(area.x + half + 1, y, area.width.saturating_sub(half + 1), 1);
        draw_split_side(
            f,
            old_rect,
            row.old.as_ref(),
            true,
            old_selected,
            view,
            options,
            t,
        );
        if let Some(cell) = f.buffer_mut().cell_mut((area.x + half, y)) {
            cell.set_symbol("│").set_fg(t.overlay0);
        }
        draw_split_side(
            f,
            new_rect,
            row.new.as_ref(),
            false,
            new_selected,
            view,
            options,
            t,
        );
        if hits.interactive {
            if let Some(number) = row.old.as_ref().and_then(|line| line.old_line) {
                if let Some(index) = stack_indices.get(&(DiffSide::Old, number)) {
                    hits.source
                        .push((hits.pane, *index, DiffSide::Old, old_rect));
                }
            }
            if let Some(number) = row.new.as_ref().and_then(|line| line.new_line) {
                if let Some(index) = stack_indices.get(&(DiffSide::New, number)) {
                    hits.source
                        .push((hits.pane, *index, DiffSide::New, new_rect));
                }
            }
        }
        y = y.saturating_add(1);
        if let Some(line) = row.old.as_ref() {
            y = draw_notes_for_anchor(
                f,
                area,
                y,
                line.old_line.map(|number| (DiffSide::Old, number)),
                &notes,
                hits,
                t,
            );
        }
        if let Some(line) = row.new.as_ref() {
            y = draw_notes_for_anchor(
                f,
                area,
                y,
                line.new_line.map(|number| (DiffSide::New, number)),
                &notes,
                hits,
                t,
            );
        }
        let selected_here = row
            .old
            .as_ref()
            .and_then(|line| line.old_line)
            .and_then(|number| stack_indices.get(&(DiffSide::Old, number)))
            .is_some_and(|index| *index == view.selected)
            || row
                .new
                .as_ref()
                .and_then(|line| line.new_line)
                .and_then(|number| stack_indices.get(&(DiffSide::New, number)))
                .is_some_and(|index| *index == view.selected);
        if selected_here && view.note_draft.is_some() {
            y = draw_note_composer(f, area, y, view, t);
        }
    }
}

fn line_anchor(line: &DiffLine) -> Option<(DiffSide, u32)> {
    line.new_line
        .map(|line| (DiffSide::New, line))
        .or_else(|| line.old_line.map(|line| (DiffSide::Old, line)))
}

fn selected_line_anchor(view: &DiffView) -> Option<(DiffSide, u32)> {
    let line = view.stack_rows.get(view.selected)?;
    match view.selected_side {
        DiffSide::Old => line.old_line.map(|number| (DiffSide::Old, number)),
        DiffSide::New => line.new_line.map(|number| (DiffSide::New, number)),
    }
    .or_else(|| line_anchor(line))
}

fn anchor_in_note_selection(view: &DiffView, side: DiffSide, number: u32) -> bool {
    if !view.note_selecting {
        return false;
    }
    let Some((current_side, current)) = selected_line_anchor(view) else {
        return false;
    };
    if current_side != side {
        return false;
    }
    let start = view
        .range_anchor
        .filter(|(anchor_side, _)| *anchor_side == side)
        .map_or(current, |(_, line)| line);
    number >= start.min(current) && number <= start.max(current)
}

fn line_in_note_selection(view: &DiffView, line: &DiffLine) -> bool {
    line.old_line
        .is_some_and(|number| anchor_in_note_selection(view, DiffSide::Old, number))
        || line
            .new_line
            .is_some_and(|number| anchor_in_note_selection(view, DiffSide::New, number))
}

fn draw_notes_for_anchor(
    f: &mut RenderTarget,
    area: Rect,
    mut y: u16,
    anchor: Option<(DiffSide, u32)>,
    notes: &DiffNotes<'_>,
    hits: &mut DiffInteractionHits<'_>,
    t: &Theme,
) -> u16 {
    let Some((side, number)) = anchor else {
        return y;
    };
    for note in notes.state.notes.iter().filter(|note| {
        note.anchor.diff_key == notes.view.key
            && note.anchor.side == side
            && number == note.anchor.end_line
            && notes.view.note_edit_id.as_deref() != Some(note.id.as_str())
    }) {
        if y >= area.bottom() {
            break;
        }
        let selected = notes.state.selected_notes.contains(&note.id);
        let marker = if selected { "◆" } else { "◇" };
        let state_label = match note.state {
            NoteState::Open => "open",
            NoteState::Resolved => "resolved",
            NoteState::Outdated => "outdated",
            NoteState::Orphaned => "orphaned",
        };
        let range = note_range_label(side, note.anchor.start_line, note.anchor.end_line);
        let title = format!(
            " {marker} {} note · {} {range} · {state_label} · [a] send ",
            note.author,
            notes.view.key.display_path()
        );
        let width = area.width.saturating_sub(6);
        if width < 20 || area.bottom().saturating_sub(y) < 3 {
            let rect = Rect::new(area.x, y, area.width, 1);
            let compact = format!(" {marker} {range} · {}", note.body.replace('\n', " ↵ "));
            f.buffer_mut().set_line(
                area.x,
                y,
                &Line::from(Span::styled(
                    clip(&compact, area.width),
                    Style::new().fg(if selected { t.text } else { t.subtext1 }),
                )),
                area.width,
            );
            if hits.interactive {
                hits.notes.push((hits.pane, note.id.clone(), rect));
            }
            y = y.saturating_add(1);
            continue;
        }

        let x = area.x + 3;
        let body_width = width.saturating_sub(4).max(1);
        let mut body = note_editor_lines(&note.body, body_width);
        if body.len() > 6 {
            body.truncate(6);
            if let Some(last) = body.last_mut() {
                *last = super::truncate(&format!("{last}…"), body_width as usize);
            }
        }
        let desired_height = body.len().saturating_add(2) as u16;
        let height = desired_height.min(area.bottom().saturating_sub(y));
        let rect = Rect::new(x, y, width, height);
        f.render_widget(
            Block::new()
                .borders(Borders::ALL)
                .title(Span::styled(
                    super::truncate(&title, width.saturating_sub(2) as usize),
                    Style::new()
                        .fg(if selected { t.text } else { t.accent })
                        .bold(),
                ))
                .style(Style::new().fg(t.border_focus).bg(t.surface0)),
            rect,
        );
        for (offset, body_line) in body
            .iter()
            .take(height.saturating_sub(2) as usize)
            .enumerate()
        {
            f.buffer_mut().set_line(
                rect.x + 2,
                rect.y + 1 + offset as u16,
                &Line::from(Span::styled(
                    body_line.as_str(),
                    Style::new().fg(t.text).bg(t.surface0),
                )),
                rect.width.saturating_sub(4),
            );
        }
        if hits.interactive {
            hits.notes.push((hits.pane, note.id.clone(), rect));
        }
        y = y.saturating_add(height);
    }
    y
}

fn draw_note_composer(f: &mut RenderTarget, area: Rect, y: u16, view: &DiffView, t: &Theme) -> u16 {
    let Some(draft) = view.note_draft.as_deref() else {
        return y;
    };
    let available = area.bottom().saturating_sub(y);
    if area.width < 24 || available < 4 {
        return y;
    }

    let width = area.width.saturating_sub(6);
    let x = area.x + 3;
    let body_width = width.saturating_sub(4).max(1);
    let lines = note_editor_lines(draft, body_width);
    let desired_height = (lines.len() as u16).saturating_add(3).clamp(5, 8);
    let height = desired_height.min(available);
    let rect = Rect::new(x, y, width, height);
    let action = if view.note_edit_id.is_some() {
        "Edit personal note"
    } else {
        "Add personal note"
    };
    let anchor = selected_line_anchor(view)
        .map(|(side, line)| {
            let start = view
                .range_anchor
                .filter(|(range_side, _)| *range_side == side)
                .map_or(line, |(_, start)| start);
            note_range_label(side, start.min(line), start.max(line))
        })
        .unwrap_or_else(|| "source".to_string());
    let title = format!(" {action} · {} {anchor} ", view.key.display_path());

    f.render_widget(
        Block::new()
            .borders(Borders::ALL)
            .title(Span::styled(
                super::truncate(&title, width.saturating_sub(2) as usize),
                Style::new().fg(t.accent).bold(),
            ))
            .style(Style::new().fg(t.border_focus).bg(t.surface0)),
        rect,
    );
    let inner = Rect::new(
        rect.x + 2,
        rect.y + 1,
        rect.width.saturating_sub(4),
        rect.height.saturating_sub(2),
    );
    let content_height = inner.height.saturating_sub(1).max(1) as usize;
    let visible_start = lines.len().saturating_sub(content_height);
    let visible = &lines[visible_start..];

    if draft.is_empty() {
        f.buffer_mut().set_line(
            inner.x,
            inner.y,
            &Line::from(Span::styled(
                "Write a private note…",
                Style::new().fg(t.overlay1).bg(t.surface0),
            )),
            inner.width,
        );
    } else {
        for (offset, line) in visible.iter().enumerate() {
            f.buffer_mut().set_line(
                inner.x,
                inner.y + offset as u16,
                &Line::from(Span::styled(
                    line.as_str(),
                    Style::new().fg(t.text).bg(t.surface0),
                )),
                inner.width,
            );
        }
    }

    let footer = "[Enter] Save locally  [Shift+Enter] New line  [Esc] Cancel";
    f.buffer_mut().set_line(
        inner.x,
        inner.bottom().saturating_sub(1),
        &Line::from(Span::styled(
            super::truncate(footer, inner.width as usize),
            Style::new().fg(t.subtext0).bg(t.surface0),
        )),
        inner.width,
    );

    let cursor_line = visible.last().map_or("", String::as_str);
    let cursor_x = inner
        .x
        .saturating_add(super::display_width(cursor_line) as u16)
        .min(inner.right().saturating_sub(1));
    let cursor_y = inner
        .y
        .saturating_add(visible.len().saturating_sub(1) as u16)
        .min(inner.bottom().saturating_sub(2));
    f.set_cursor_position((cursor_x, cursor_y));
    y.saturating_add(height)
}

fn note_range_label(side: DiffSide, start: u32, end: u32) -> String {
    let prefix = match side {
        DiffSide::Old => 'L',
        DiffSide::New => 'R',
    };
    if start == end {
        format!("{prefix}{start}")
    } else {
        format!("{prefix}{start}-{prefix}{end}")
    }
}

fn note_editor_lines(text: &str, width: u16) -> Vec<String> {
    text.split('\n')
        .flat_map(|line| wrapped_text(line, width))
        .collect()
}

fn draw_agent_picker(f: &mut RenderTarget, area: Rect, picker: &DiffAgentPicker, t: &Theme) {
    let width = area.width.saturating_sub(4).clamp(20, 72);
    let height = (picker.choices.len() as u16 + 4).min(area.height).max(5);
    let rect = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    f.render_widget(Clear, rect);
    f.render_widget(
        Block::new()
            .borders(Borders::ALL)
            .title(" Send review notes ")
            .style(Style::new().fg(t.border_focus).bg(t.mantle)),
        rect,
    );
    let inner = Rect::new(
        rect.x + 1,
        rect.y + 1,
        rect.width.saturating_sub(2),
        rect.height.saturating_sub(2),
    );
    f.render_widget(
        Paragraph::new(Span::styled(
            format!("Scope: {}  [Tab] change", picker.scope.label()),
            Style::new().fg(t.accent),
        )),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    for (index, choice) in picker
        .choices
        .iter()
        .enumerate()
        .take(inner.height.saturating_sub(2) as usize)
    {
        let selected = index == picker.cursor;
        f.render_widget(
            Paragraph::new(Span::styled(
                clip(
                    &format!("{} {}", if selected { "▸" } else { " " }, choice.label),
                    inner.width,
                ),
                Style::new()
                    .fg(if selected { t.text } else { t.subtext0 })
                    .bg(if selected { t.surface1 } else { t.mantle }),
            )),
            Rect::new(inner.x, inner.y + 1 + index as u16, inner.width, 1),
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_split_side(
    f: &mut RenderTarget,
    area: Rect,
    line: Option<&DiffLine>,
    old_side: bool,
    selected: bool,
    view: &DiffView,
    options: DiffRenderOptions,
    t: &Theme,
) {
    let Some(line) = line else {
        f.render_widget(Block::new().style(Style::new().bg(t.mantle)), area);
        return;
    };
    let number = if old_side {
        line.old_line
    } else {
        line.new_line
    };
    let number = if view.show_line_numbers {
        format!("{:>5} ", number.map_or(String::new(), |n| n.to_string()))
    } else {
        String::new()
    };
    let (bar, symbol) = gutter_markers(line.kind, options.marker_style);
    let style = line_style(line.kind, selected, options.color_mode, t);
    let text = horizontal_text(
        &line.text,
        view.horizontal,
        area.width.saturating_sub(
            (bar.chars().count() + number.chars().count() + symbol.chars().count()) as u16,
        ),
    );
    fill_bg(f, area, style.bg);
    f.buffer_mut().set_line(
        area.x,
        area.y,
        &Line::from(vec![
            Span::styled(bar, Style::new().fg(style.marker).bg(style.bg).bold()),
            Span::styled(number, Style::new().fg(t.overlay1).bg(style.bg)),
            Span::styled(symbol, Style::new().fg(style.marker).bg(style.bg).bold()),
            Span::styled(text, Style::new().fg(style.text).bg(style.bg)),
        ]),
        area.width,
    );
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct DiffLineStyle {
    text: Color,
    marker: Color,
    bg: Color,
}

fn line_marker(kind: DiffLineKind) -> &'static str {
    match kind {
        DiffLineKind::Addition => "+",
        DiffLineKind::Deletion => "-",
        // The Git-provided hunk text already starts with `@@`; another marker
        // here rendered as the confusing `@ @@ ...`.
        DiffLineKind::Header => " ",
        DiffLineKind::NoNewline => "\\",
        DiffLineKind::Context => " ",
    }
}

fn gutter_markers(kind: DiffLineKind, style: DiffMarkerStyle) -> (String, String) {
    let bar = if style.shows_bars() {
        match kind {
            DiffLineKind::Addition | DiffLineKind::Deletion => "▎".to_string(),
            _ => " ".to_string(),
        }
    } else {
        String::new()
    };
    let symbol = if style.shows_symbols() {
        format!("{} ", line_marker(kind))
    } else {
        String::new()
    };
    (bar, symbol)
}

fn line_style(
    kind: DiffLineKind,
    selected: bool,
    color_mode: DiffColorMode,
    t: &Theme,
) -> DiffLineStyle {
    let (addition, deletion) = change_colors(color_mode, t);
    match kind {
        DiffLineKind::Addition => DiffLineStyle {
            text: t.text,
            marker: addition,
            bg: semantic_surface(t.mantle, addition, selected, t),
        },
        DiffLineKind::Deletion => DiffLineStyle {
            text: t.text,
            marker: deletion,
            bg: semantic_surface(t.mantle, deletion, selected, t),
        },
        DiffLineKind::Header => DiffLineStyle {
            text: t.accent,
            marker: t.accent,
            bg: if selected { t.surface1 } else { t.mantle },
        },
        DiffLineKind::NoNewline => DiffLineStyle {
            text: t.overlay1,
            marker: t.overlay1,
            bg: if selected { t.surface1 } else { t.mantle },
        },
        DiffLineKind::Context => DiffLineStyle {
            text: if selected { t.text } else { t.subtext0 },
            marker: t.overlay1,
            bg: if selected { t.surface1 } else { t.mantle },
        },
    }
}

fn change_colors(mode: DiffColorMode, t: &Theme) -> (Color, Color) {
    match mode {
        DiffColorMode::Theme => (t.mint, t.coral),
        // Familiar GitHub-style review colors remain fixed across themes while
        // their row surfaces are still blended against the active background.
        DiffColorMode::Standard => (Color::Rgb(63, 185, 80), Color::Rgb(248, 81, 73)),
    }
}

fn semantic_surface(base: Color, accent: Color, selected: bool, t: &Theme) -> Color {
    let fallback = if selected { t.surface1 } else { t.surface0 };
    let (Color::Rgb(br, bg, bb), Color::Rgb(ar, ag, ab)) = (base, accent) else {
        return fallback;
    };
    let percent = if selected { 34 } else { 22 };
    let blend =
        |from: u8, to: u8| (from as i16 + ((to as i16 - from as i16) * percent) / 100) as u8;
    Color::Rgb(blend(br, ar), blend(bg, ag), blend(bb, ab))
}

fn fill_bg(f: &mut RenderTarget, rect: Rect, color: Color) {
    let buffer = f.buffer_mut();
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            buffer[(x, y)].set_bg(color);
        }
    }
}

fn horizontal_text(text: &str, offset: usize, width: u16) -> String {
    text.chars().skip(offset).take(width as usize).collect()
}

fn wrapped_text(text: &str, width: u16) -> Vec<String> {
    let width = width.max(1) as usize;
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    chars
        .chunks(width)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

fn center(f: &mut RenderTarget, area: Rect, text: &str, color: Color) {
    if area.height == 0 {
        return;
    }
    let y = area.y + area.height / 2;
    f.buffer_mut().set_line(
        area.x,
        y,
        &Line::from(Span::styled(clip(text, area.width), Style::new().fg(color))),
        area.width,
    );
}

fn clip(text: &str, width: u16) -> String {
    text.chars().take(width as usize).collect()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use ratatui::buffer::Buffer;

    use super::*;
    use crate::diff::{DiffKey, DiffLayer, RepoPath};

    fn test_view(rows: Vec<DiffLine>) -> DiffView {
        let path = RepoPath::from_path(Path::new("src/lib.rs")).unwrap();
        let mut view = DiffView::new(
            PathBuf::from("/repo"),
            DiffKey {
                repo_id: "repo".into(),
                worktree_id: "tree".into(),
                layer: DiffLayer::Worktree,
                old_path: Some(path.clone()),
                new_path: Some(path),
            },
            DiffLayoutPreference::Stack,
            3,
            false,
            false,
        );
        view.stack_rows = rows;
        view
    }

    fn changed_line(kind: DiffLineKind, text: &str) -> DiffLine {
        DiffLine {
            kind,
            old_line: (kind == DiffLineKind::Deletion).then_some(1),
            new_line: (kind == DiffLineKind::Addition).then_some(1),
            text: text.into(),
        }
    }

    fn options(marker_style: DiffMarkerStyle) -> DiffRenderOptions {
        DiffRenderOptions {
            marker_style,
            color_mode: DiffColorMode::Theme,
        }
    }

    #[test]
    fn auto_and_split_fall_back_at_narrow_widths() {
        assert_eq!(
            effective_layout(DiffLayoutPreference::Auto, SPLIT_MIN_WIDTH - 1),
            DiffLayoutPreference::Stack
        );
        assert_eq!(
            effective_layout(DiffLayoutPreference::Split, SPLIT_MIN_WIDTH - 1),
            DiffLayoutPreference::Stack
        );
        assert_eq!(
            effective_layout(DiffLayoutPreference::Auto, SPLIT_MIN_WIDTH),
            DiffLayoutPreference::Split
        );
    }

    #[test]
    fn wrapping_is_unicode_safe_and_never_returns_zero_rows() {
        assert_eq!(wrapped_text("", 0), vec![""]);
        assert_eq!(wrapped_text("abçd", 2), vec!["ab", "çd"]);
    }

    #[test]
    fn personal_note_composer_renders_inside_the_diff_and_owns_the_cursor() {
        let theme = Theme::quattro_rally();
        let area = Rect::new(0, 0, 80, 15);
        let mut view = test_view(vec![changed_line(DiffLineKind::Addition, "new")]);
        view.note_draft = Some("Check this behavior\nbefore merging".into());
        let mut buffer = Buffer::empty(area);
        let cursor = {
            let mut target = RenderTarget::new(&mut buffer, area);
            let end = draw_note_composer(&mut target, area, 1, &view, &theme);
            assert!(end > 1, "the inline editor consumes layout rows");
            target.cursor()
        };
        let screen: String = buffer.content().iter().map(|cell| cell.symbol()).collect();

        assert!(screen.contains("Add personal note"));
        assert!(screen.contains("Check this behavior"));
        assert!(screen.contains("before merging"));
        assert!(screen.contains("Save locally"));
        assert!(
            cursor.is_some(),
            "typing cursor is placed inside the editor"
        );
        assert_eq!(
            note_editor_lines("one\ntwo", 20),
            vec!["one".to_string(), "two".to_string()]
        );
    }

    #[test]
    fn inline_composer_pushes_following_source_rows_down() {
        let theme = Theme::quattro_rally();
        let mut view = test_view(vec![
            DiffLine {
                kind: DiffLineKind::Context,
                old_line: Some(1),
                new_line: Some(1),
                text: "before".into(),
            },
            DiffLine {
                kind: DiffLineKind::Addition,
                old_line: None,
                new_line: Some(2),
                text: "annotated".into(),
            },
            DiffLine {
                kind: DiffLineKind::Context,
                old_line: Some(2),
                new_line: Some(3),
                text: "after".into(),
            },
        ]);
        view.selected = 1;
        view.note_draft = Some("Explain this change".into());
        let area = Rect::new(0, 0, 80, 12);
        let pane = crate::ids::PaneId(11);
        let mut buffer = Buffer::empty(area);
        let mut rects = Vec::new();
        {
            let mut target = RenderTarget::new(&mut buffer, area);
            let mut note_rects = Vec::new();
            let mut hits = DiffInteractionHits {
                pane,
                interactive: false,
                source: &mut rects,
                notes: &mut note_rects,
            };
            draw_stack(
                &mut target,
                area,
                &view,
                &DiffState::default(),
                options(DiffMarkerStyle::Symbols),
                &mut hits,
                &theme,
            );
        }
        let row_text = |y| -> String { (0..area.width).map(|x| buffer[(x, y)].symbol()).collect() };

        assert!(row_text(2).contains("Add personal note"));
        assert!(row_text(7).contains("after"));
        assert!(!row_text(2).contains("after"));
    }

    #[test]
    fn saved_note_card_pushes_following_source_rows_down() {
        let theme = Theme::quattro_rally();
        let rows = vec![
            DiffLine {
                kind: DiffLineKind::Addition,
                old_line: None,
                new_line: Some(2),
                text: "annotated".into(),
            },
            DiffLine {
                kind: DiffLineKind::Context,
                old_line: Some(2),
                new_line: Some(3),
                text: "after".into(),
            },
        ];
        let view = test_view(rows);
        let mut state = DiffState::default();
        state.notes.push(crate::diff::ReviewNote {
            id: "note-inline".into(),
            review_id: "review".into(),
            author: "user".into(),
            kind: crate::diff::NoteKind::Issue,
            body: "Explain this change".into(),
            anchor: crate::diff::notes::NoteAnchor {
                diff_key: view.key.clone(),
                side: DiffSide::New,
                start_line: 2,
                end_line: 2,
                context: "annotated".into(),
                context_sha256: "hash".into(),
            },
            state: NoteState::Open,
            deliveries: Vec::new(),
            revision: 1,
            created_at_ms: 1,
            updated_at_ms: 1,
        });
        let area = Rect::new(0, 0, 80, 10);
        let pane = crate::ids::PaneId(12);
        let mut buffer = Buffer::empty(area);
        let mut rects = Vec::new();
        let mut note_rects = Vec::new();
        {
            let mut target = RenderTarget::new(&mut buffer, area);
            let mut hits = DiffInteractionHits {
                pane,
                interactive: true,
                source: &mut rects,
                notes: &mut note_rects,
            };
            draw_stack(
                &mut target,
                area,
                &view,
                &state,
                options(DiffMarkerStyle::Symbols),
                &mut hits,
                &theme,
            );
        }
        let row_text = |y| -> String { (0..area.width).map(|x| buffer[(x, y)].symbol()).collect() };

        assert!(row_text(1).contains("user note"));
        assert!(row_text(4).contains("after"));
        assert_eq!(rects[1].3.y, 4);
        assert_eq!(
            note_rects,
            vec![(pane, "note-inline".to_string(), Rect::new(3, 1, 74, 3))]
        );
    }

    #[test]
    fn header_keeps_metadata_right_aligned_and_colors_change_counts() {
        let theme = Theme::quattro_rally();
        let area = Rect::new(0, 0, 60, 1);
        let metadata_text = "WORKTREE  +1 -2  11/32  SPLIT";
        let metadata = Line::from(vec![
            Span::styled("WORKTREE  ", Style::new().fg(theme.overlay1)),
            Span::styled("+1", Style::new().fg(theme.mint)),
            Span::styled(" ", Style::new().fg(theme.overlay1)),
            Span::styled("-2", Style::new().fg(theme.coral)),
            Span::styled("  11/32  SPLIT", Style::new().fg(theme.overlay1)),
        ]);
        let mut buffer = Buffer::empty(area);
        {
            let mut target = RenderTarget::new(&mut buffer, area);
            draw_diff_header(&mut target, area, "src/main.rs", metadata, &theme);
        }

        let screen: String = (0..area.width).map(|x| buffer[(x, 0)].symbol()).collect();
        assert!(screen.starts_with(" src/main.rs"));
        assert!(screen.ends_with(metadata_text));
        let metadata_x = area.width - super::super::display_width(metadata_text) as u16;
        let plus_x = metadata_x + "WORKTREE  ".len() as u16;
        let minus_x = plus_x + "+1 ".len() as u16;
        assert_eq!(buffer[(plus_x, 0)].fg, theme.mint);
        assert_eq!(buffer[(minus_x, 0)].fg, theme.coral);
    }

    #[test]
    fn change_markers_are_explicit_in_both_layouts() {
        assert_eq!(line_marker(DiffLineKind::Addition), "+");
        assert_eq!(line_marker(DiffLineKind::Deletion), "-");
        assert_eq!(line_marker(DiffLineKind::Context), " ");
    }

    #[test]
    fn semantic_surfaces_preserve_addition_and_deletion_colors() {
        let theme = Theme::quattro_rally();
        let addition = line_style(DiffLineKind::Addition, false, DiffColorMode::Theme, &theme);
        let deletion = line_style(DiffLineKind::Deletion, false, DiffColorMode::Theme, &theme);
        let selected_addition =
            line_style(DiffLineKind::Addition, true, DiffColorMode::Theme, &theme);

        assert_ne!(addition.bg, theme.mantle);
        assert_ne!(deletion.bg, theme.mantle);
        assert_ne!(addition.bg, deletion.bg);
        assert_ne!(addition.bg, selected_addition.bg);
        assert_eq!(addition.marker, theme.mint);
        assert_eq!(deletion.marker, theme.coral);
    }

    #[test]
    fn standard_color_mode_uses_stable_red_and_green_on_every_theme() {
        for theme in [Theme::quattro_rally(), Theme::sky()] {
            let addition = line_style(
                DiffLineKind::Addition,
                false,
                DiffColorMode::Standard,
                &theme,
            );
            let deletion = line_style(
                DiffLineKind::Deletion,
                false,
                DiffColorMode::Standard,
                &theme,
            );

            assert_eq!(addition.marker, Color::Rgb(63, 185, 80));
            assert_eq!(deletion.marker, Color::Rgb(248, 81, 73));
            assert_ne!(addition.bg, deletion.bg);
            assert_ne!(addition.bg, theme.mantle);
            assert_ne!(deletion.bg, theme.mantle);
        }
    }

    #[test]
    fn stack_renders_markers_and_tints_the_full_changed_rows() {
        let theme = Theme::quattro_rally();
        let rows = vec![
            changed_line(DiffLineKind::Addition, "new"),
            changed_line(DiffLineKind::Deletion, "old"),
        ];
        let view = test_view(rows);
        let area = Rect::new(0, 0, 20, 2);
        let mut buffer = Buffer::empty(area);
        let pane = crate::ids::PaneId(7);
        let mut rects = Vec::new();
        {
            let mut target = RenderTarget::new(&mut buffer, area);
            let mut note_rects = Vec::new();
            let mut hits = DiffInteractionHits {
                pane,
                interactive: true,
                source: &mut rects,
                notes: &mut note_rects,
            };
            draw_stack(
                &mut target,
                area,
                &view,
                &DiffState::default(),
                options(DiffMarkerStyle::Symbols),
                &mut hits,
                &theme,
            );
        }

        assert_eq!(buffer[(0, 0)].symbol(), "+");
        assert_eq!(buffer[(0, 1)].symbol(), "-");
        assert_eq!(
            buffer[(19, 0)].bg,
            line_style(DiffLineKind::Addition, true, DiffColorMode::Theme, &theme).bg
        );
        assert_eq!(
            buffer[(19, 1)].bg,
            line_style(DiffLineKind::Deletion, false, DiffColorMode::Theme, &theme).bg
        );
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0], (pane, 0, DiffSide::New, Rect::new(0, 0, 20, 1)));
        assert_eq!(rects[1], (pane, 1, DiffSide::Old, Rect::new(0, 1, 20, 1)));
    }

    #[test]
    fn split_renders_addition_and_deletion_markers_without_line_numbers() {
        let theme = Theme::quattro_rally();
        let view = test_view(Vec::new());
        let area = Rect::new(0, 0, 21, 1);
        let mut buffer = Buffer::empty(area);
        let deletion = changed_line(DiffLineKind::Deletion, "old");
        let addition = changed_line(DiffLineKind::Addition, "new");
        {
            let mut target = RenderTarget::new(&mut buffer, area);
            draw_split_side(
                &mut target,
                Rect::new(0, 0, 10, 1),
                Some(&deletion),
                true,
                false,
                &view,
                options(DiffMarkerStyle::Symbols),
                &theme,
            );
            draw_split_side(
                &mut target,
                Rect::new(11, 0, 10, 1),
                Some(&addition),
                false,
                false,
                &view,
                options(DiffMarkerStyle::Symbols),
                &theme,
            );
        }

        assert_eq!(buffer[(0, 0)].symbol(), "-");
        assert_eq!(buffer[(11, 0)].symbol(), "+");
        assert_eq!(
            buffer[(9, 0)].bg,
            line_style(DiffLineKind::Deletion, false, DiffColorMode::Theme, &theme).bg
        );
        assert_eq!(
            buffer[(20, 0)].bg,
            line_style(DiffLineKind::Addition, false, DiffColorMode::Theme, &theme).bg
        );
    }

    #[test]
    fn split_source_hits_preserve_the_clicked_side_and_stack_identity() {
        let theme = Theme::quattro_rally();
        let old = DiffLine {
            kind: DiffLineKind::Deletion,
            old_line: Some(7),
            new_line: None,
            text: "old".into(),
        };
        let new = DiffLine {
            kind: DiffLineKind::Addition,
            old_line: None,
            new_line: Some(8),
            text: "new".into(),
        };
        let mut view = test_view(vec![old.clone(), new.clone()]);
        view.split_rows.push(crate::diff::rows::SplitRow {
            old: Some(old),
            new: Some(new),
        });
        let area = Rect::new(0, 0, 101, 1);
        let pane = crate::ids::PaneId(9);
        let mut buffer = Buffer::empty(area);
        let mut rects = Vec::new();
        {
            let mut target = RenderTarget::new(&mut buffer, area);
            let mut note_rects = Vec::new();
            let mut hits = DiffInteractionHits {
                pane,
                interactive: true,
                source: &mut rects,
                notes: &mut note_rects,
            };
            draw_split(
                &mut target,
                area,
                &view,
                &DiffState::default(),
                options(DiffMarkerStyle::Symbols),
                &mut hits,
                &theme,
            );
        }

        assert_eq!(
            rects,
            vec![
                (pane, 0, DiffSide::Old, Rect::new(0, 0, 50, 1)),
                (pane, 1, DiffSide::New, Rect::new(51, 0, 50, 1)),
            ]
        );
    }

    #[test]
    fn split_renders_old_side_notes_with_their_exact_anchor() {
        let theme = Theme::quattro_rally();
        let context = DiffLine {
            kind: DiffLineKind::Context,
            old_line: Some(7),
            new_line: Some(8),
            text: "same".into(),
        };
        let mut view = test_view(vec![context.clone()]);
        view.split_rows.push(crate::diff::rows::SplitRow {
            old: Some(context.clone()),
            new: Some(context),
        });
        let mut state = DiffState::default();
        state.notes.push(crate::diff::ReviewNote {
            id: "note-1".into(),
            review_id: "review".into(),
            author: "user".into(),
            kind: crate::diff::NoteKind::Issue,
            body: "Keep the old behavior".into(),
            anchor: crate::diff::notes::NoteAnchor {
                diff_key: view.key.clone(),
                side: DiffSide::Old,
                start_line: 7,
                end_line: 7,
                context: "same".into(),
                context_sha256: "hash".into(),
            },
            state: NoteState::Open,
            deliveries: Vec::new(),
            revision: 1,
            created_at_ms: 1,
            updated_at_ms: 1,
        });
        let area = Rect::new(0, 0, 101, 5);
        let mut buffer = Buffer::empty(area);
        let mut rects = Vec::new();
        {
            let mut target = RenderTarget::new(&mut buffer, area);
            let mut note_rects = Vec::new();
            let mut hits = DiffInteractionHits {
                pane: crate::ids::PaneId(10),
                interactive: true,
                source: &mut rects,
                notes: &mut note_rects,
            };
            draw_split(
                &mut target,
                area,
                &view,
                &state,
                options(DiffMarkerStyle::Symbols),
                &mut hits,
                &theme,
            );
        }
        let screen: String = buffer.content().iter().map(|cell| cell.symbol()).collect();

        assert!(screen.contains("user note · src/lib.rs L7 · open"));
        assert!(screen.contains("Keep the old behavior"));
    }

    #[test]
    fn marker_styles_offer_symbols_bars_and_both() {
        assert_eq!(
            gutter_markers(DiffLineKind::Addition, DiffMarkerStyle::Symbols),
            (String::new(), "+ ".to_string())
        );
        assert_eq!(
            gutter_markers(DiffLineKind::Deletion, DiffMarkerStyle::Bars),
            ("▎".to_string(), String::new())
        );
        assert_eq!(
            gutter_markers(DiffLineKind::Addition, DiffMarkerStyle::Both),
            ("▎".to_string(), "+ ".to_string())
        );
        assert_eq!(
            gutter_markers(DiffLineKind::Context, DiffMarkerStyle::Bars),
            (" ".to_string(), String::new())
        );

        let theme = Theme::quattro_rally();
        let area = Rect::new(0, 0, 12, 1);
        let render = |marker_style| {
            let mut buffer = Buffer::empty(area);
            let view = test_view(vec![changed_line(DiffLineKind::Addition, "new")]);
            let mut rects = Vec::new();
            {
                let mut target = RenderTarget::new(&mut buffer, area);
                let mut note_rects = Vec::new();
                let mut hits = DiffInteractionHits {
                    pane: crate::ids::PaneId(8),
                    interactive: true,
                    source: &mut rects,
                    notes: &mut note_rects,
                };
                draw_stack(
                    &mut target,
                    area,
                    &view,
                    &DiffState::default(),
                    options(marker_style),
                    &mut hits,
                    &theme,
                );
            }
            buffer
        };
        let bars = render(DiffMarkerStyle::Bars);
        assert_eq!(bars[(0, 0)].symbol(), "▎");
        assert_eq!(bars[(1, 0)].symbol(), "n");
        let both = render(DiffMarkerStyle::Both);
        assert_eq!(both[(0, 0)].symbol(), "▎");
        assert_eq!(both[(1, 0)].symbol(), "+");
    }
}
