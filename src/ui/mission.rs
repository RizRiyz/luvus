//! Mission Control render (docs/54): a full-tab command deck for the workspace's
//! live and resumable agents. Data is precomputed into `MissionRowView`s by
//! `App::build_mission_rows`, so the responsive Ratatui grid borrows no `App`.

use std::borrow::Cow;

use super::*;
use crate::i18n::Catalog;
use crate::mission::{MissionRowView, MissionScope};

/// Format a token count compactly: `945`, `12.3k`, `1.2M`.
fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
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

/// Compact USD: `$3.20`, `$1.2k`, `$24.9k`.
fn fmt_cost(c: f64) -> String {
    if c >= 1000.0 {
        format!("${:.1}k", c / 1000.0)
    } else {
        format!("${c:.2}")
    }
}

/// ASCII case-insensitive substring test that allocates nothing. `needle` must
/// already be lowercase (every caller passes a literal).
fn contains_ignore_case(hay: &str, needle: &str) -> bool {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    if n.is_empty() || h.len() < n.len() {
        return n.is_empty();
    }
    h.windows(n.len())
        .any(|w| w.iter().zip(n).all(|(a, b)| a.to_ascii_lowercase() == *b))
}

/// A short model tag for the model column (`opus`, `sonnet`, `gpt-4o`, …), else a
/// truncated id.
fn short_model(m: &str) -> Cow<'static, str> {
    // Match without folding the whole string: this runs per row per frame while
    // the tab is open, and a known model hits one of the borrowed arms below, so
    // the common case allocates nothing at all.
    for k in ["opus", "sonnet", "haiku", "gpt-5", "gpt-4o", "o3", "o1"] {
        if contains_ignore_case(m, k) {
            return Cow::Borrowed(k);
        }
    }
    if m.is_empty() {
        Cow::Borrowed("")
    } else {
        Cow::Owned(truncate(m, 8))
    }
}

fn deck_block(title: &str, t: &Theme, focus: bool) -> ratatui::widgets::Block<'static> {
    use ratatui::widgets::{Block, BorderType, Borders};
    let border = if focus { t.border_focus } else { t.surface1 };
    Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::new().fg(border).bg(t.mantle))
        .title(Span::styled(
            format!(" {title} "),
            Style::new()
                .fg(if focus { t.accent } else { t.overlay1 })
                .bold(),
        ))
        .style(Style::new().bg(t.mantle))
}

fn pad_right(text: &str, width: usize) -> String {
    let text = truncate(text, width);
    format!(
        "{text}{}",
        " ".repeat(width.saturating_sub(display_width(&text)))
    )
}

fn pad_left(text: &str, width: usize) -> String {
    let text = truncate(text, width);
    format!(
        "{}{text}",
        " ".repeat(width.saturating_sub(display_width(&text)))
    )
}

fn meter(value: f32, width: usize) -> (String, String) {
    let width = width.max(1);
    let fill = ((value.clamp(0.0, 1.0) * width as f32).round() as usize).min(width);
    ("█".repeat(fill), "░".repeat(width - fill))
}

fn draw_header(
    f: &mut RenderTarget,
    area: Rect,
    rows: &[MissionRowView],
    scope: MissionScope,
    cat: &Catalog,
    t: &Theme,
) {
    if area.height == 0 {
        return;
    }
    let working = rows.iter().filter(|r| r.state == State::Working).count();
    let blocked = rows.iter().filter(|r| r.state == State::Blocked).count();
    let signal = if blocked > 0 {
        ("ATTENTION", t.coral)
    } else if working > 0 {
        ("ACTIVE", t.mint)
    } else {
        ("STANDBY", t.overlay1)
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" ◉ ", Style::new().fg(signal.1).bold()),
            Span::styled(cat.mc_title, Style::new().fg(t.text).bold()),
            Span::raw("  "),
            Span::styled(" BETA ", Style::new().fg(t.crust).bg(t.accent).bold()),
            Span::styled(
                match scope {
                    MissionScope::Workspace => "  //  CURRENT WORKSPACE",
                    MissionScope::All => "  //  ALL WORKSPACES",
                },
                Style::new().fg(t.overlay1),
            ),
        ])),
        Rect::new(area.x, area.y, area.width, 1),
    );
    let status = if area.width >= 72 {
        format!(
            "STATE {0}  ·  AGENTS {1:02}  ·  BLOCKED {2:02}",
            signal.0,
            rows.len(),
            blocked
        )
    } else {
        signal.0.to_string()
    };
    let sw = status.chars().count().min(area.width as usize) as u16;
    f.render_widget(
        Paragraph::new(Span::styled(status, Style::new().fg(signal.1))),
        Rect::new(area.right().saturating_sub(sw + 1), area.y, sw, 1),
    );
    if area.height > 1 {
        let scan = "─".repeat(area.width.saturating_sub(2) as usize);
        f.render_widget(
            Paragraph::new(Span::styled(scan, Style::new().fg(t.surface1))),
            Rect::new(area.x + 1, area.y + 1, area.width.saturating_sub(2), 1),
        );
    }
}

fn draw_scope_tabs(
    f: &mut RenderTarget,
    area: Rect,
    scope: MissionScope,
    refreshing: bool,
    cat: &Catalog,
    t: &Theme,
) -> (Vec<(MissionScope, Rect)>, Option<Rect>) {
    if area.height == 0 || area.width < 8 {
        return (Vec::new(), None);
    }
    let labels = [
        (MissionScope::Workspace, cat.workspace.to_uppercase()),
        (
            MissionScope::All,
            format!(
                "{} {}",
                cat.all.to_uppercase(),
                cat.workspaces.to_uppercase()
            ),
        ),
    ];
    let mut x = area.x.saturating_add(1);
    let mut hits = Vec::with_capacity(labels.len());
    for (kind, label) in labels {
        let text = format!(" {label} ");
        let width = display_width(&text).min(area.right().saturating_sub(x) as usize) as u16;
        if width == 0 {
            break;
        }
        let rect = Rect::new(x, area.y, width, 1);
        let selected = kind == scope;
        f.render_widget(
            Paragraph::new(Span::styled(
                truncate(&text, width as usize),
                Style::new()
                    .fg(if selected { t.base } else { t.overlay1 })
                    .bg(if selected { t.accent } else { t.mantle })
                    .bold(),
            )),
            rect,
        );
        hits.push((kind, rect));
        x = x.saturating_add(width).saturating_add(1);
        if x >= area.right() {
            break;
        }
    }
    let refresh_text = if refreshing {
        " REFRESHING "
    } else {
        " REFRESH "
    };
    let refresh_width = display_width(refresh_text) as u16;
    let refresh_rect = area
        .width
        .checked_sub(refresh_width)
        .map(|offset| Rect::new(area.x + offset, area.y, refresh_width, 1))
        .filter(|rect| rect.x > x);
    if let Some(rect) = refresh_rect {
        f.render_widget(
            Paragraph::new(Span::styled(
                refresh_text,
                Style::new()
                    .fg(if refreshing { t.base } else { t.overlay1 })
                    .bg(if refreshing { t.accent } else { t.mantle })
                    .bold(),
            )),
            rect,
        );
    }
    (hits, refresh_rect)
}

fn draw_metric(
    f: &mut RenderTarget,
    area: Rect,
    title: &str,
    value: String,
    detail: String,
    color: Color,
    t: &Theme,
) {
    if area.width < 4 || area.height < 3 {
        return;
    }
    let block = deck_block(title, t, false);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 {
        return;
    }
    let mut lines = vec![Line::from(Span::styled(
        format!(" {value}"),
        Style::new().fg(color).bold(),
    ))];
    if inner.height > 1 {
        lines.push(Line::from(Span::styled(
            format!(
                " {}",
                truncate(&detail, inner.width.saturating_sub(1) as usize)
            ),
            Style::new().fg(t.overlay1),
        )));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_metrics(
    f: &mut RenderTarget,
    area: Rect,
    rows: &[MissionRowView],
    burn: Option<f64>,
    budget: Option<f64>,
    cat: &Catalog,
    t: &Theme,
) {
    if area.height < 3 {
        return;
    }
    let cells: [Rect; 4] = Layout::horizontal([
        Constraint::Percentage(25),
        Constraint::Percentage(25),
        Constraint::Percentage(25),
        Constraint::Percentage(25),
    ])
    .areas(area);
    let working = rows.iter().filter(|r| r.state == State::Working).count();
    let blocked = rows.iter().filter(|r| r.state == State::Blocked).count();
    let resumable = rows.iter().filter(|r| r.resumable).count();
    let total_cost: f64 = rows.iter().filter_map(|r| r.usage.as_ref()?.cost).sum();
    let over_budget = budget.is_some_and(|b| total_cost > b);
    draw_metric(
        f,
        cells[0],
        &cat.mc_agents.to_uppercase(),
        format!("{:02}", rows.len()),
        format!(
            "{} live · {} {}",
            rows.len().saturating_sub(resumable),
            rows.len(),
            cat.mc_total
        ),
        t.accent,
        t,
    );
    draw_metric(
        f,
        cells[1],
        &cat.mc_working.to_uppercase(),
        format!("{:02}", working),
        "currently working".into(),
        if working > 0 { t.mint } else { t.overlay1 },
        t,
    );
    draw_metric(
        f,
        cells[2],
        &cat.mc_blocked.to_uppercase(),
        format!("{:02}", blocked),
        if blocked > 0 {
            "awaiting response"
        } else {
            "no blocked agents"
        }
        .into(),
        if blocked > 0 { t.coral } else { t.green },
        t,
    );
    let burn_detail = burn
        .filter(|r| *r >= 0.005)
        .map(|r| format!("{}/hr", fmt_cost(r)))
        .unwrap_or_else(|| "rate unavailable".into());
    draw_metric(
        f,
        cells[3],
        "SPEND",
        fmt_cost(total_cost),
        burn_detail,
        if over_budget { t.coral } else { t.green },
        t,
    );
}

#[derive(Clone, Copy, Debug)]
struct AgentColumns {
    number: usize,
    agent: usize,
    state: usize,
    location: usize,
    model: usize,
    tokens: usize,
    context: usize,
    cost: usize,
}

impl AgentColumns {
    fn for_width(width: usize) -> Self {
        const MARKER: usize = 2;
        if width >= 81 {
            // Wide terminals keep a compact, left-aligned table. Spare space
            // stays after COST instead of splitting the table in the middle.
            Self {
                number: 3,
                agent: 12,
                state: 10,
                location: 18,
                model: 10,
                tokens: 9,
                context: 8,
                cost: 9,
            }
        } else if width >= 73 {
            // Full table with LOCATION shrinking only when space is constrained.
            Self {
                number: 3,
                agent: 12,
                state: 10,
                location: width - 63,
                model: 10,
                tokens: 9,
                context: 8,
                cost: 9,
            }
        } else if width >= 63 {
            // Keep the most useful usage fields and drop context first.
            Self {
                number: 3,
                agent: 12,
                state: 10,
                location: width - 55,
                model: 10,
                tokens: 9,
                context: 0,
                cost: 9,
            }
        } else if width >= 37 {
            // Narrow table: identity and location remain aligned.
            Self {
                number: 3,
                agent: 12,
                state: 10,
                location: width - (MARKER + 3 + 12 + 10),
                model: 0,
                tokens: 0,
                context: 0,
                cost: 0,
            }
        } else {
            // Tiny fallback: never overflow the terminal.
            Self {
                number: 3.min(width.saturating_sub(MARKER)),
                agent: width.saturating_sub(MARKER + 3),
                state: 0,
                location: 0,
                model: 0,
                tokens: 0,
                context: 0,
                cost: 0,
            }
        }
    }

    fn total(self) -> usize {
        2 + self.number
            + self.agent
            + self.state
            + self.location
            + self.model
            + self.tokens
            + self.context
            + self.cost
    }
}

fn column_right(text: &str, width: usize) -> String {
    if width == 0 {
        String::new()
    } else {
        format!("{} ", pad_right(text, width - 1))
    }
}

fn column_left(text: &str, width: usize) -> String {
    if width == 0 {
        String::new()
    } else {
        format!("{} ", pad_left(text, width - 1))
    }
}

fn draw_agent_header(f: &mut RenderTarget, area: Rect, columns: AgentColumns, t: &Theme) {
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(
            column_right("#", columns.number),
            Style::new().fg(t.overlay0),
        ),
        Span::styled(
            column_right("AGENT", columns.agent),
            Style::new().fg(t.overlay0).bold(),
        ),
    ];
    for (label, width) in [
        ("STATE", columns.state),
        ("LOCATION", columns.location),
        ("MODEL", columns.model),
    ] {
        if width > 0 {
            spans.push(Span::styled(
                column_right(label, width),
                Style::new().fg(t.overlay0).bold(),
            ));
        }
    }
    for (label, width) in [
        ("TOKENS", columns.tokens),
        ("CONTEXT", columns.context),
        ("COST", columns.cost),
    ] {
        if width > 0 {
            spans.push(Span::styled(
                column_left(label, width),
                Style::new().fg(t.overlay0).bold(),
            ));
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_agent_row(
    f: &mut RenderTarget,
    area: Rect,
    row: &MissionRowView,
    number: usize,
    selected: bool,
    columns: AgentColumns,
    t: &Theme,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    if selected {
        fill_bg(f, area, t.surface1);
    }
    let state_color = if row.resumable {
        t.overlay1
    } else {
        row.state.color(t)
    };
    let rail = Rect::new(area.x, area.y, 1, 1);
    fill_bg(f, rail, state_color);
    let state = if row.resumable {
        "RESUME"
    } else {
        row.state.label()
    };
    let usage = row.usage.as_ref();
    let model = usage
        .map(|usage| short_model(&usage.model))
        .unwrap_or(Cow::Borrowed("—"));
    let tokens = usage
        .map(|usage| fmt_tokens(usage.total_tokens()))
        .unwrap_or_else(|| "—".into());
    let context = usage
        .and_then(|usage| usage.context)
        .map(|context| format!("{}%", (context * 100.0).round() as u32))
        .unwrap_or_else(|| "—".into());
    let cost = usage
        .and_then(|usage| usage.cost)
        .map(fmt_cost)
        .unwrap_or_else(|| "—".into());
    let mut spans = vec![
        Span::styled(
            if selected { "▸ " } else { "  " },
            Style::new().fg(t.accent),
        ),
        Span::styled(
            column_left(&format!("{number:02}"), columns.number),
            Style::new().fg(t.overlay1),
        ),
        Span::styled(
            column_right(&row.agent.to_uppercase(), columns.agent),
            Style::new().fg(t.text).bold(),
        ),
    ];
    if columns.state > 0 {
        spans.push(Span::styled(
            column_right(state, columns.state),
            Style::new().fg(state_color),
        ));
    }
    if columns.location > 0 {
        spans.push(Span::styled(
            column_right(&row.location, columns.location),
            Style::new().fg(t.overlay1),
        ));
    }
    if columns.model > 0 {
        spans.push(Span::styled(
            column_right(model.as_ref(), columns.model),
            Style::new().fg(t.mint),
        ));
    }
    for (value, width, color) in [
        (&tokens, columns.tokens, t.subtext1),
        (&context, columns.context, t.subtext1),
        (&cost, columns.cost, t.green),
    ] {
        if width > 0 {
            spans.push(Span::styled(
                column_left(value, width),
                Style::new().fg(color),
            ));
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[derive(Default)]
struct RosterRender {
    scroll: usize,
    row_rects: Vec<(usize, Rect)>,
}

fn draw_roster(
    f: &mut RenderTarget,
    area: Rect,
    rows: &[MissionRowView],
    cursor: usize,
    requested_scroll: usize,
    cat: &Catalog,
    t: &Theme,
) -> RosterRender {
    let block = deck_block("AGENT SESSIONS", t, true);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 {
        return RosterRender::default();
    }
    let content = Rect::new(
        inner.x.saturating_add(2),
        inner.y,
        inner.width.saturating_sub(2),
        1,
    );
    let columns = AgentColumns::for_width(content.width as usize);
    debug_assert!(columns.total() <= content.width as usize);
    draw_agent_header(f, content, columns, t);
    if rows.is_empty() || inner.height < 2 {
        f.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled("        ──╂──", Style::new().fg(t.surface1))),
                Line::from(Span::styled(
                    format!("      {}", cat.mc_empty.to_uppercase()),
                    Style::new().fg(t.overlay1).bold(),
                )),
            ]),
            Rect::new(
                inner.x,
                inner.y.saturating_add(1),
                inner.width,
                inner.height.saturating_sub(1),
            ),
        );
        return RosterRender::default();
    }
    let rows_area = Rect::new(inner.x, inner.y + 1, inner.width, inner.height - 1);
    let visible = rows_area.height.max(1) as usize;
    let cursor = cursor.min(rows.len().saturating_sub(1));
    let mut scroll = requested_scroll.min(rows.len().saturating_sub(1));
    if cursor < scroll {
        scroll = cursor;
    } else if cursor >= scroll + visible {
        scroll = cursor + 1 - visible;
    }
    scroll = scroll.min(rows.len().saturating_sub(visible));
    let mut row_rects = Vec::with_capacity(visible.min(rows.len()));
    for (slot, idx) in (scroll..rows.len().min(scroll + visible)).enumerate() {
        let rect = Rect::new(rows_area.x, rows_area.y + slot as u16, rows_area.width, 1);
        draw_agent_row(f, rect, &rows[idx], idx + 1, idx == cursor, columns, t);
        row_rects.push((idx, rect));
    }
    RosterRender { scroll, row_rects }
}

fn draw_selected(f: &mut RenderTarget, area: Rect, row: Option<&MissionRowView>, t: &Theme) {
    let block = deck_block("SELECTED AGENT", t, false);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(row) = row else {
        return;
    };
    let color = if row.resumable {
        t.overlay1
    } else {
        row.state.color(t)
    };
    let state = if row.resumable {
        "RESUMABLE"
    } else {
        row.state.label()
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled(" ◉ ", Style::new().fg(color)),
            Span::styled(row.agent.to_uppercase(), Style::new().fg(t.text).bold()),
            Span::styled(format!("  {state}"), Style::new().fg(color)),
        ]),
        Line::from(Span::styled(
            format!(
                "   WORKSPACE  {}",
                truncate(&row.location, inner.width.saturating_sub(10) as usize)
            ),
            Style::new().fg(t.overlay1),
        )),
    ];
    if let Some(u) = &row.usage {
        lines.push(Line::from(vec![
            Span::styled("   MODEL  ", Style::new().fg(t.overlay0)),
            Span::styled(truncate(&u.model, 24), Style::new().fg(t.mint)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("   I/O    ", Style::new().fg(t.overlay0)),
            Span::styled(
                format!(
                    "{} in · {} out · {} cache",
                    fmt_tokens(u.tokens_in),
                    fmt_tokens(u.tokens_out),
                    fmt_tokens(u.cache)
                ),
                Style::new().fg(t.subtext1),
            ),
        ]));
        if let Some(context) = u.context {
            let meter_w = inner.width.saturating_sub(17).min(18) as usize;
            let (on, off) = meter(context, meter_w);
            let warning = context >= crate::mission::COMPACT_AT;
            lines.push(Line::from(vec![
                Span::styled("   CONTEXT ", Style::new().fg(t.overlay0)),
                Span::styled(on, Style::new().fg(if warning { t.coral } else { t.mint })),
                Span::styled(off, Style::new().fg(t.surface1)),
                Span::styled(
                    format!(" {:>3}%", (context * 100.0).round() as u32),
                    Style::new().fg(if warning { t.coral } else { t.subtext1 }),
                ),
            ]));
        }
    } else {
        lines.push(Line::from(Span::styled(
            "   USAGE  data unavailable",
            Style::new().fg(t.overlay0),
        )));
    }
    if let Some(hint) = &row.blocked_hint {
        lines.push(Line::from(Span::styled(
            format!(
                "   ALERT  {}",
                truncate(hint, inner.width.saturating_sub(10) as usize)
            ),
            Style::new().fg(t.coral),
        )));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_signal_matrix(f: &mut RenderTarget, area: Rect, rows: &[MissionRowView], t: &Theme) {
    let block = deck_block("AGENT STATUS", t, false);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let states = [State::Working, State::Blocked, State::Done, State::Idle];
    let mut spans = Vec::new();
    for state in states {
        let count = rows
            .iter()
            .filter(|r| !r.resumable && r.state == state)
            .count();
        spans.push(Span::styled(
            format!(" {} {} {:02} ", state.dot(), state.label(), count),
            Style::new().fg(state.color(t)),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), inner);
}

fn draw_cost_chart(f: &mut RenderTarget, area: Rect, rows: &[MissionRowView], t: &Theme) {
    let block = deck_block("COST BY MODEL", t, false);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 {
        return;
    }
    let mut map: Vec<(Cow<'static, str>, f64)> = Vec::new();
    for row in rows {
        if let Some(cost) = row.usage.as_ref().and_then(|u| u.cost) {
            let model = short_model(&row.usage.as_ref().unwrap().model);
            match map.iter_mut().find(|(name, _)| *name == model) {
                Some(entry) => entry.1 += cost,
                None => map.push((model, cost)),
            }
        }
    }
    if map.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(
                " cost data unavailable",
                Style::new().fg(t.overlay0),
            )),
            inner,
        );
        return;
    }
    map.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let max_cost = map[0].1.max(1e-9);
    // Keep a blank terminal row between bars. Adjacent full-block glyphs merge
    // visually into one large rectangle, especially when several models are
    // present. Showing fewer distinct rows is easier to scan than packing them.
    let row_stride = 2_u16;
    let visible = inner.height.div_ceil(row_stride) as usize;
    for (line, (model, cost)) in map.iter().take(visible).enumerate() {
        let bar_w = inner.width.saturating_sub(18).max(1) as usize;
        let fill = ((cost / max_cost * bar_w as f64).round() as usize).min(bar_w);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!(" {:<7}", truncate(model, 7)),
                    Style::new().fg(t.mint),
                ),
                Span::styled("█".repeat(fill), Style::new().fg(t.accent)),
                Span::styled("░".repeat(bar_w - fill), Style::new().fg(t.surface1)),
                Span::styled(format!(" {:>7}", fmt_cost(*cost)), Style::new().fg(t.green)),
            ])),
            Rect::new(inner.x, inner.y + line as u16 * row_stride, inner.width, 1),
        );
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render(
    f: &mut RenderTarget,
    area: Rect,
    rows: &[MissionRowView],
    scroll: usize,
    cursor: usize,
    scope: MissionScope,
    refreshing: bool,
    burn: Option<f64>,
    budget: Option<f64>,
    compact: bool,
    cat: &Catalog,
    t: &Theme,
) -> MissionRender {
    if area.height < 4 || area.width < 24 {
        return MissionRender {
            scroll: 0,
            scope_rects: Vec::new(),
            refresh_rect: None,
            row_rects: Vec::new(),
        };
    }
    fill_bg(f, area, t.mantle);
    let footer_h = u16::from(!compact && area.height >= 10);
    let [header, scopes, metrics, body, footer]: [Rect; 5] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Length(if area.height >= 14 { 4 } else { 3 }),
        Constraint::Min(3),
        Constraint::Length(footer_h),
    ])
    .areas(area);
    draw_header(f, header, rows, scope, cat, t);
    let (scope_rects, refresh_rect) = draw_scope_tabs(f, scopes, scope, refreshing, cat, t);
    draw_metrics(f, metrics, rows, burn, budget, cat, t);

    let cursor = cursor.min(rows.len().saturating_sub(1));
    let rendered = if body.width >= 78 && body.height >= 14 {
        let [roster, telemetry]: [Rect; 2] =
            Layout::horizontal([Constraint::Percentage(64), Constraint::Percentage(36)])
                .areas(body);
        let selected_h = (telemetry.height / 2).clamp(7, 10);
        let [selected, matrix, chart]: [Rect; 3] = Layout::vertical([
            Constraint::Length(selected_h),
            Constraint::Length(3),
            Constraint::Min(3),
        ])
        .areas(telemetry);
        let rendered = draw_roster(f, roster, rows, cursor, scroll, cat, t);
        draw_selected(f, selected, rows.get(cursor), t);
        draw_signal_matrix(f, matrix, rows, t);
        draw_cost_chart(f, chart, rows, t);
        rendered
    } else {
        draw_roster(f, body, rows, cursor, scroll, cat, t)
    };
    if footer_h > 0 {
        f.render_widget(
            Paragraph::new(super::hint_line(
                &[
                    ("↑↓", "navigate"),
                    ("tab", "scope"),
                    ("r", cat.act_refresh),
                    ("⏎", cat.mc_go),
                    ("a", cat.mc_answer),
                    ("i", cat.mc_stop),
                    ("x", cat.act_close),
                    ("o", cat.board_details),
                ],
                t,
            )),
            footer,
        );
    }
    MissionRender {
        scroll: rendered.scroll,
        scope_rects,
        refresh_rect,
        row_rects: rendered.row_rects,
    }
}

pub(super) struct MissionRender {
    pub scroll: usize,
    pub scope_rects: Vec<(MissionScope, Rect)>,
    pub refresh_rect: Option<Rect>,
    pub row_rects: Vec<(usize, Rect)>,
}

/// The row-detail overlay (MC-5): a small modal with the selected agent's full
/// breakdown — model, tokens, context and estimated cost. Read-only; any of
/// esc/o/q/⏎ closes it. Drawn last, over a dimmed backdrop like the other modals.
pub(super) fn draw_detail(
    f: &mut RenderTarget,
    area: Rect,
    r: &MissionRowView,
    cat: &Catalog,
    t: &Theme,
) {
    use ratatui::widgets::{Block, Borders, Clear};
    super::help::dim_backdrop(f, area, t);
    let w = area.width.saturating_sub(6).clamp(40, 64).min(area.width);
    let modal = super::help::centered_rect(area, w, 16);
    f.render_widget(Clear, modal);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(t.border_focus).bg(t.surface0))
        .style(Style::new().bg(t.surface0));
    let inner = block.inner(modal);
    f.render_widget(block, modal);

    let kv = |k: &str, v: String| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!(" {k:<9}"), Style::new().fg(t.subtext0)),
            Span::styled(v, Style::new().fg(t.text)),
        ])
    };
    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            format!(" {} — {}", cat.mc_title, r.agent),
            Style::new().fg(t.text).bold(),
        )),
        Line::from(""),
    ];
    let status = if r.resumable {
        "resumable".to_string()
    } else {
        r.state.label().to_string()
    };
    lines.push(kv("status", status));
    lines.push(kv("where", r.location.clone()));
    match &r.usage {
        Some(u) => {
            if !u.model.is_empty() {
                lines.push(kv("model", u.model.clone()));
            }
            lines.push(kv("input", format!("{} tok", fmt_tokens(u.tokens_in))));
            lines.push(kv("output", format!("{} tok", fmt_tokens(u.tokens_out))));
            lines.push(kv("cache", format!("{} tok", fmt_tokens(u.cache))));
            if let Some(c) = u.context {
                let headroom = ((crate::mission::COMPACT_AT - c) * 100.0).max(0.0).round() as u32;
                lines.push(kv(
                    "context",
                    format!(
                        "{}% used · {}% until compact",
                        (c * 100.0).round() as u32,
                        headroom
                    ),
                ));
            }
            if let Some(cost) = u.cost {
                lines.push(kv("cost", format!("${cost:.2} (estimate)")));
            }
        }
        None => lines.push(Line::from(Span::styled(
            "  no usage data for this session",
            Style::new().fg(t.overlay0),
        ))),
    }
    // What it's blocked on, if anything.
    if let Some(hint) = &r.blocked_hint {
        lines.push(Line::from(""));
        lines.push(kv(
            "waiting",
            truncate(hint, inner.width.saturating_sub(11) as usize),
        ));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("  esc · {}", cat.act_close),
        Style::new().fg(t.overlay0),
    )));
    f.render_widget(Paragraph::new(lines), inner);
}

/// The inline "answer the agent" input (docs/54): a one-line prompt to type a
/// reply that is sent to the selected blocked agent's pane. `⏎` sends, `esc`
/// cancels. Drawn last, over a dimmed backdrop.
pub(super) fn draw_answer(f: &mut RenderTarget, area: Rect, text: &str, cat: &Catalog, t: &Theme) {
    use ratatui::widgets::{Block, Borders, Clear};
    super::help::dim_backdrop(f, area, t);
    let w = area.width.saturating_sub(6).clamp(40, 72).min(area.width);
    let modal = super::help::centered_rect(area, w, 5);
    f.render_widget(Clear, modal);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(t.border_focus).bg(t.surface0))
        .style(Style::new().bg(t.surface0));
    let inner = block.inner(modal);
    f.render_widget(block, modal);
    let shown = truncate(text, inner.width.saturating_sub(4) as usize);
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                format!(" {}", cat.mc_answer),
                Style::new().fg(t.text).bold(),
            )),
            Line::from(vec![
                Span::styled(" > ", Style::new().fg(t.overlay1)),
                Span::styled(format!("{shown}▏"), Style::new().fg(t.text)),
            ]),
            Line::from(Span::styled(
                format!("  ⏎ · esc {}", cat.act_cancel),
                Style::new().fg(t.overlay0),
            )),
        ]),
        inner,
    );
}

#[cfg(test)]
mod tests {
    use super::{display_width, pad_left, pad_right, AgentColumns};

    #[test]
    fn agent_columns_keep_exact_display_widths() {
        let short_agent = pad_right("FX", 14);
        let long_agent = pad_right("A-VERY-LONG-AGENT", 14);
        let tokens = pad_left("9.3M", 8);
        let context = pad_left("60%", 4);

        assert_eq!(display_width(&short_agent), 14);
        assert_eq!(display_width(&long_agent), 14);
        assert_eq!(display_width(&tokens), 8);
        assert_eq!(display_width(&context), 4);
        assert!(tokens.ends_with("9.3M"));
        assert!(context.ends_with("60%"));

        for width in 20..=160 {
            assert_eq!(
                AgentColumns::for_width(width).total(),
                width.min(81),
                "the table stays compact without overflowing at {width} columns"
            );
        }
    }
}
