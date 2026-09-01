//! Ratatui rendering for prevalidated Luvus Bar state.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use super::{
    BarHit, BarLayout, BarRegion, BarSegment, BarSegmentKind, BarState, BarTone, OverflowHit,
    WidgetCandidate,
};
use crate::ui::theme::{State, Theme};
use crate::ui::RenderTarget;
use std::borrow::Cow;

pub fn draw_region(
    f: &mut RenderTarget,
    area: Rect,
    region: BarRegion,
    candidates: &[WidgetCandidate<'_>],
    layout: &BarLayout,
    t: &Theme,
) -> (Vec<BarHit>, Option<OverflowHit>) {
    if area.width == 0 || area.height == 0 || layout.is_empty() {
        return (Vec::new(), None);
    }
    let mut hits = Vec::new();
    let mut x = area.right().saturating_sub(layout.width.min(area.width));
    for (position, item) in layout.items.iter().enumerate() {
        if position > 0 {
            x = x.saturating_add(2);
        }
        let candidate = &candidates[item.candidate];
        for (segment_index, segment) in candidate
            .widget
            .segments(item.representation)
            .iter()
            .enumerate()
        {
            let width = segment.display_width() as u16;
            let rect = Rect::new(x, area.y, width.min(area.right().saturating_sub(x)), 1);
            if rect.width == 0 {
                continue;
            }
            draw_segment(
                f,
                rect,
                segment,
                candidate.key == super::CORE_RUNTIME && segment_index == 0,
                t,
            );
            if let Some(action) = segment.action.as_ref() {
                hits.push(BarHit {
                    key: candidate.widget.key.clone(),
                    segment: segment_index,
                    rect,
                    action: action.clone(),
                    value: segment.value.clone(),
                });
            }
            x = x.saturating_add(width);
        }
    }

    let overflow = if layout.overflow_width > 0 {
        if !layout.items.is_empty() {
            x = x.saturating_add(2);
        }
        let rect = Rect::new(
            x,
            area.y,
            layout.overflow_width.min(area.right().saturating_sub(x)),
            1,
        );
        let text = format!("… +{}", layout.hidden.len());
        f.render_widget(
            Paragraph::new(Span::styled(text, Style::new().fg(t.accent).bold())),
            rect,
        );
        Some(OverflowHit {
            region,
            rect,
            hidden: layout
                .hidden
                .iter()
                .map(|index| candidates[*index].widget.key.canonical())
                .collect(),
        })
    } else {
        None
    };
    (hits, overflow)
}

fn draw_segment(
    f: &mut RenderTarget,
    rect: Rect,
    segment: &BarSegment,
    core_runtime_label: bool,
    t: &Theme,
) {
    let (text, state_color): (Cow<'_, str>, Option<Color>) = match &segment.kind {
        BarSegmentKind::Text { text } | BarSegmentKind::Symbol { symbol: text } => {
            (Cow::Borrowed(text), None)
        }
        BarSegmentKind::State { state, label } => {
            let state = parse_state(state);
            let glyph = state.dot();
            let text = label
                .as_ref()
                .map(|label| format!("{glyph} {label}"))
                .unwrap_or_else(|| glyph.to_string());
            (Cow::Owned(text), Some(state.color(t)))
        }
        BarSegmentKind::Badge { text } => (Cow::Owned(format!("[{text}]")), None),
        BarSegmentKind::Progress {
            value,
            total,
            width,
        } => {
            let inner = width.saturating_sub(2) as usize;
            let filled = ((*value as u128 * inner as u128) / *total as u128) as usize;
            (
                Cow::Owned(format!(
                    "[{}{}]",
                    "━".repeat(filled),
                    "─".repeat(inner - filled)
                )),
                None,
            )
        }
        BarSegmentKind::Spacer { width } => (Cow::Owned(" ".repeat(*width as usize)), None),
        BarSegmentKind::Separator => (Cow::Borrowed("  ·  "), None),
    };
    let color = if core_runtime_label {
        t.overlay1
    } else {
        state_color.unwrap_or_else(|| tone_color(segment.tone, t))
    };
    let style = if core_runtime_label {
        Style::new().fg(color).bold()
    } else {
        Style::new().fg(color)
    };
    f.render_widget(Paragraph::new(Span::styled(text, style)), rect);
}

fn parse_state(state: &str) -> State {
    match state {
        "blocked" => State::Blocked,
        "working" => State::Working,
        "done" => State::Done,
        "idle" => State::Idle,
        _ => State::Unknown,
    }
}

fn tone_color(tone: BarTone, t: &Theme) -> Color {
    match tone {
        BarTone::Normal => t.subtext0,
        BarTone::Muted => t.overlay0,
        BarTone::Accent => t.accent,
        BarTone::Success => t.mint,
        BarTone::Warning => t.amber,
        BarTone::Error => t.coral,
    }
}

fn overflow_height(rows: usize, area_height: u16, region: BarRegion) -> u16 {
    let requested = (rows as u16 + 4).clamp(5, 18);
    match region {
        BarRegion::TopRight => requested.min(area_height.saturating_sub(1)),
        BarRegion::BottomRight => requested.min(area_height),
    }
}

pub fn draw_overflow(f: &mut RenderTarget, area: Rect, state: &mut BarState, t: &Theme) {
    let Some(open) = state.overflow.as_ref() else {
        return;
    };
    let region = open.region;
    let keys = open.keys.clone();
    let rows: Vec<String> = keys.iter().map(|key| state.title(key)).collect();
    let widest = rows
        .iter()
        .map(|row| crate::ui::display_width(row))
        .max()
        .unwrap_or(12);
    let width = (widest as u16 + 6).clamp(24, 60).min(area.width);
    // The top popup starts one row below the bar, so that anchor row is not
    // available to its height. Keeping the offset in this cap prevents the modal
    // from crossing the viewport bottom by one cell.
    let height = overflow_height(rows.len(), area.height, region);
    let x = area.right().saturating_sub(width + 2).max(area.x);
    let y = match region {
        BarRegion::TopRight => area.y.saturating_add(1),
        BarRegion::BottomRight => area.bottom().saturating_sub(height + 1),
    };
    let modal = Rect::new(x, y, width, height);
    f.render_widget(Clear, modal);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(t.border_focus).bg(t.surface0))
        .style(Style::new().bg(t.surface0));
    let inner = block.inner(modal);
    f.render_widget(block, modal);
    f.render_widget(
        Paragraph::new(Span::styled(
            format!(
                " Luvus Bar · {}",
                match region {
                    BarRegion::TopRight => "Top",
                    BarRegion::BottomRight => "Bottom",
                }
            ),
            Style::new().fg(t.text).bold(),
        )),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    for (index, row) in rows
        .iter()
        .take(inner.height.saturating_sub(2) as usize)
        .enumerate()
    {
        f.render_widget(
            Paragraph::new(Span::styled(
                format!(
                    "  · {}",
                    crate::ui::truncate(row, inner.width.saturating_sub(4) as usize)
                ),
                Style::new().fg(t.subtext1),
            )),
            Rect::new(inner.x, inner.y + 1 + index as u16, inner.width, 1),
        );
    }
    f.render_widget(
        Paragraph::new(Span::styled(" esc close", Style::new().fg(t.overlay0))),
        Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
    );
    if let Some(open) = state.overflow.as_mut() {
        open.rect = modal;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;

    #[test]
    fn working_state_uses_static_filled_marker() {
        let area = Rect::new(0, 0, 16, 1);
        let mut buffer = Buffer::empty(area);
        let mut target = RenderTarget::new(&mut buffer, area);
        let segment = BarSegment {
            kind: BarSegmentKind::State {
                state: "working".into(),
                label: Some("agent".into()),
            },
            tone: BarTone::Normal,
            action: None,
            value: None,
        };
        draw_segment(
            &mut target,
            area,
            &segment,
            false,
            &crate::ui::theme::by_name("quattro-rally"),
        );

        let rendered = (0..area.width)
            .map(|x| buffer.cell((x, 0)).map(|cell| cell.symbol()).unwrap_or(" "))
            .collect::<String>();
        assert!(rendered.starts_with("● agent"));
    }

    #[test]
    fn top_overflow_reserves_its_anchor_row_without_losing_a_fitting_item() {
        let height = overflow_height(5, 10, BarRegion::TopRight);
        assert_eq!(height, 9);
        assert_eq!(1 + height, 10, "popup ends exactly at area.bottom()");
        assert_eq!(height.saturating_sub(4), 5, "all five list rows fit");
        assert_eq!(
            overflow_height(20, 10, BarRegion::BottomRight),
            10,
            "bottom behavior still uses the full available height"
        );
    }
}
