//! The orchestration board dashboard (docs/22, ORCH-7): a header with task
//! counts, an interactive list of tasks (status dot · id · state · title · deps ·
//! assignee), the active path leases, and the new-task form. Pure ratatui,
//! localized through the i18n catalog (docs/21), and built from the same panel
//! chrome as Mission Control. Rendered from the shared `OrchState`.

use super::*;
use crate::app::OrchForm;
use crate::i18n::Catalog;
use crate::orch::{OrchState, Task, TaskStatus};
use ratatui::widgets::{Borders, Clear, Wrap};

/// A task's status, localized for display (the English `TaskStatus::as_str` stays
/// the wire/JSON form; this is the human-facing label, docs/21).
fn status_label(s: TaskStatus, cat: &Catalog) -> &'static str {
    match s {
        TaskStatus::Queued => cat.task_queued,
        TaskStatus::Claimed => cat.task_claimed,
        TaskStatus::Running => cat.task_running,
        TaskStatus::Blocked => cat.task_blocked,
        TaskStatus::Review => cat.task_review,
        TaskStatus::Done => cat.task_done,
        TaskStatus::Merging => cat.task_merging,
        TaskStatus::Merged => cat.task_merged,
        TaskStatus::Failed => cat.task_failed,
    }
}

/// Color for a task's status dot/label.
fn status_color(s: TaskStatus, t: &Theme) -> Color {
    match s {
        TaskStatus::Queued => t.overlay0,
        TaskStatus::Claimed => t.subtext0,
        TaskStatus::Running => t.amber,
        TaskStatus::Blocked => t.coral,
        TaskStatus::Review => t.amber,
        TaskStatus::Done => t.green,
        TaskStatus::Merging => t.accent,
        TaskStatus::Merged => t.green,
        TaskStatus::Failed => t.coral,
    }
}

fn status_dot(s: TaskStatus) -> &'static str {
    match s {
        TaskStatus::Queued => "○",
        TaskStatus::Done => "●",
        TaskStatus::Merged => "◆",
        TaskStatus::Failed => "✗",
        TaskStatus::Blocked => "⏸",
        _ => "◐",
    }
}

#[derive(Default)]
pub(super) struct BoardRender {
    pub scroll: usize,
    pub hits: Vec<(crate::app::OrchHit, Rect)>,
}

/// Renders the board and returns its clamped scroll plus visible hit geometry.
#[allow(clippy::too_many_arguments)]
pub(super) fn render(
    f: &mut RenderTarget,
    area: Rect,
    orch: &OrchState,
    scroll: usize,
    cursor: usize,
    flow_mode: crate::orch::TaskWorkerMode,
    compact: bool,
    hover: Option<(u16, u16)>,
    cat: &Catalog,
    t: &Theme,
) -> BoardRender {
    if area.height < 4 || area.width < 16 {
        return BoardRender::default();
    }
    fill_bg(f, area, t.mantle);
    let mut hits = Vec::new();
    // Match the established full-tab dashboard header: identity and action on
    // the first row, fleet signal below it, then a quiet separator.
    let mut counts = [0usize; 9];
    for task in &orch.tasks {
        counts[status_index(task.status)] += 1;
    }
    let action_text = format!(" + {} ", cat.board_new_task.to_uppercase());
    let action_w = (super::display_width(&action_text) as u16).min(area.width);
    let action = Rect::new(
        area.right().saturating_sub(action_w.saturating_add(1)),
        area.y,
        action_w,
        1,
    );
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" ◎ ", Style::new().fg(t.accent).bold()),
            Span::styled(cat.board_title, Style::new().fg(t.text).bold()),
            Span::styled(
                format!("  //  {}", cat.board_tasks.to_uppercase()),
                Style::new().fg(t.overlay1),
            ),
        ])),
        Rect::new(
            area.x,
            area.y,
            area.width.saturating_sub(action_w.saturating_add(2)),
            1,
        ),
    );
    let action_hot = row_is_hovered(action, hover);
    f.render_widget(
        Paragraph::new(Span::styled(
            action_text,
            Style::new()
                .fg(if action_hot { t.base } else { t.accent })
                .bg(if action_hot { t.accent } else { t.mantle })
                .bold(),
        )),
        action,
    );
    hits.push((crate::app::OrchHit::NewTask, action));
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!("   {}  ", cat.col_status.to_uppercase()),
                Style::new().fg(t.overlay0),
            ),
            Span::styled(
                fmt_count(cat.task_queued, counts[0]),
                Style::new().fg(t.overlay1),
            ),
            Span::styled("  ·  ", Style::new().fg(t.surface1)),
            Span::styled(
                fmt_count(cat.task_running, counts[2] + counts[1] + counts[6]),
                Style::new().fg(if counts[2] + counts[1] + counts[6] > 0 {
                    t.mint
                } else {
                    t.overlay1
                }),
            ),
            Span::styled("  ·  ", Style::new().fg(t.surface1)),
            Span::styled(
                fmt_count(cat.task_blocked, counts[3]),
                Style::new().fg(if counts[3] > 0 { t.coral } else { t.overlay1 }),
            ),
            Span::styled("  ·  ", Style::new().fg(t.surface1)),
            Span::styled(
                fmt_count(cat.task_done, counts[5] + counts[7]),
                Style::new().fg(if counts[5] + counts[7] > 0 {
                    t.green
                } else {
                    t.overlay1
                }),
            ),
        ])),
        Rect::new(area.x, area.y.saturating_add(1), area.width, 1),
    );
    hline(f, area.x + 1, area.y + 2, area.width.saturating_sub(2), t);

    let footer_h = u16::from(!compact && area.height >= 10);
    let body_y = area.y.saturating_add(3);
    let body = Rect::new(
        area.x,
        body_y,
        area.width,
        area.bottom().saturating_sub(body_y + footer_h),
    );
    if body.height == 0 {
        return BoardRender { scroll: 0, hits };
    }

    let wide = !compact
        && body.width >= crate::app::ORCH_INLINE_DETAIL_MIN_WIDTH
        && area.height >= crate::app::ORCH_INLINE_DETAIL_MIN_HEIGHT;
    if footer_h > 0 {
        let mut hints = vec![
            ("a", cat.act_new),
            ("s", cat.board_start),
            ("d", cat.task_done),
        ];
        if orch.tasks.get(cursor).and_then(|task| task.worker_mode)
            != Some(crate::orch::TaskWorkerMode::Workspace)
        {
            hints.push(("m", cat.act_merge));
        }
        hints.extend([("⏎", cat.pane), ("o", cat.board_details)]);
        hints.extend([
            ("x", cat.board_release),
            ("D", cat.act_delete),
            ("q", cat.act_close),
        ]);
        f.render_widget(
            Paragraph::new(super::hint_line(&hints, t)),
            Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
        );
    }

    // A wide board keeps the task fleet visible while exposing the selected
    // task's useful context. Narrow clients keep the full body for task rows.
    let (left, detail) = if wide {
        let left_w = ((u32::from(body.width) * 64 / 100) as u16).max(56);
        (
            Rect::new(body.x, body.y, left_w, body.height),
            Some(Rect::new(
                body.x.saturating_add(left_w),
                body.y,
                body.width.saturating_sub(left_w),
                body.height,
            )),
        )
    } else {
        (body, None)
    };

    let lease_h = if compact || left.height < 10 {
        0
    } else {
        ((orch.leases.len() as u16).saturating_add(3))
            .clamp(3, 6)
            .min(left.height / 3)
    };
    let task_area = Rect::new(
        left.x,
        left.y,
        left.width,
        left.height.saturating_sub(lease_h),
    );
    let lease_area = (lease_h > 0).then(|| {
        Rect::new(
            left.x,
            left.bottom().saturating_sub(lease_h),
            left.width,
            lease_h,
        )
    });

    let task_block = super::dashboard_block(
        format!("{} {:02}", cat.board_tasks.to_uppercase(), orch.tasks.len()),
        t,
        true,
    );
    let task_inner = task_block.inner(task_area);
    f.render_widget(task_block, task_area);

    if orch.tasks.is_empty() {
        draw_empty(f, task_inner, cat, t);
        if let Some(leases) = lease_area {
            draw_leases(f, leases, orch, cat, t);
        }
        if let Some(flow) = detail {
            hits.extend(draw_flow(f, flow, flow_mode, cat, t));
        }
        return BoardRender { scroll: 0, hits };
    }

    // Render a real table header on desktop. Compact clients keep every row for
    // tasks and rely on the same column alignment without spending a line on
    // labels.
    let columns = task_columns(task_inner.width as usize, cat);
    let header_h = u16::from(!compact && task_inner.height > 1);
    if header_h > 0 {
        draw_task_header(
            f,
            Rect::new(task_inner.x, task_inner.y, task_inner.width, 1),
            &columns,
            cat,
            t,
        );
    }
    let rows_area = Rect::new(
        task_inner.x,
        task_inner.y.saturating_add(header_h),
        task_inner.width,
        task_inner.height.saturating_sub(header_h),
    );

    // Render row-by-row so selected and hovered task rows get a restrained
    // full-width tint and the selected row gets an explicit accent marker.
    // Only visible rows become hit targets.
    let task_count = orch.tasks.len();
    let vis = rows_area.height as usize;
    let cursor = cursor.min(task_count.saturating_sub(1));
    let mut scroll = scroll;
    if cursor < scroll {
        scroll = cursor;
    } else if cursor >= scroll + vis {
        scroll = cursor + 1 - vis;
    }
    scroll = scroll.min(task_count.saturating_sub(vis));
    for (row, i) in (scroll..task_count.min(scroll + vis)).enumerate() {
        let rect = Rect::new(rows_area.x, rows_area.y + row as u16, rows_area.width, 1);
        let hot = row_is_hovered(rect, hover);
        let selected = i == cursor;
        if selected || hot {
            fill_bg(f, rect, t.surface0);
        }
        let task = &orch.tasks[i];
        let rendered = task_line(task, &columns, selected, cat, t);
        f.render_widget(Paragraph::new(rendered.line), rect);
        if let Some(col) = rendered.worker_col {
            let worker = Rect::new(
                rect.x.saturating_add(col as u16),
                rect.y,
                rect.width.saturating_sub(col as u16),
                1,
            );
            if worker.width > 0 {
                hits.push((crate::app::OrchHit::Worker(task.id.clone()), worker));
            }
        }
        hits.push((crate::app::OrchHit::Task(task.id.clone()), rect));
    }
    if let Some(leases) = lease_area {
        draw_leases(f, leases, orch, cat, t);
    }
    if let (Some(detail), Some(task)) = (detail, orch.tasks.get(cursor)) {
        draw_summary(f, detail, task, cat, t);
    }

    BoardRender { scroll, hits }
}

fn row_is_hovered(row: Rect, hover: Option<(u16, u16)>) -> bool {
    hover.is_some_and(|(column, pointer_row)| {
        column >= row.x
            && column < row.right()
            && pointer_row >= row.y
            && pointer_row < row.bottom()
    })
}

#[derive(Clone, Copy)]
struct TaskColumns {
    marker: usize,
    id: usize,
    status: usize,
    title: usize,
    deps: usize,
    mode: usize,
    worker: usize,
}

impl TaskColumns {
    fn worker_col(self) -> Option<usize> {
        (self.worker > 0)
            .then(|| self.marker + self.id + self.status + self.title + self.deps + self.mode)
    }
}

fn task_columns(width: usize, cat: &Catalog) -> TaskColumns {
    let marker = 2;
    let id = 5;
    let status_label_w = [
        cat.col_status,
        cat.task_queued,
        cat.task_claimed,
        cat.task_running,
        cat.task_blocked,
        cat.task_review,
        cat.task_done,
        cat.task_merging,
        cat.task_merged,
        cat.task_failed,
    ]
    .into_iter()
    .map(super::display_width)
    .max()
    .unwrap_or(6);
    let status = (status_label_w + 3).clamp(9, 15);
    let deps = if width >= 92 { 12 } else { 0 };
    let mode = if width >= 96 { 12 } else { 0 };
    let worker = if width >= 112 {
        34
    } else if width >= 78 {
        26
    } else if width >= 62 {
        20
    } else {
        0
    };
    let fixed = marker + id + status + deps + mode + worker;
    let title = width.saturating_sub(fixed).max(4);
    TaskColumns {
        marker,
        id,
        status,
        title,
        deps,
        mode,
        worker,
    }
}

fn draw_task_header(
    f: &mut RenderTarget,
    area: Rect,
    columns: &TaskColumns,
    cat: &Catalog,
    t: &Theme,
) {
    let mut spans = vec![
        Span::raw(" ".repeat(columns.marker)),
        Span::styled(pad("ID", columns.id), Style::new().fg(t.overlay1).bold()),
        Span::styled(
            pad(&cat.col_status.to_uppercase(), columns.status),
            Style::new().fg(t.overlay1).bold(),
        ),
        Span::styled(
            pad(&cat.board_tasks.to_uppercase(), columns.title),
            Style::new().fg(t.overlay1).bold(),
        ),
    ];
    if columns.deps > 0 {
        spans.push(Span::styled(
            pad(&cat.board_f_deps.to_uppercase(), columns.deps),
            Style::new().fg(t.overlay1).bold(),
        ));
    }
    if columns.mode > 0 {
        spans.push(Span::styled(
            pad(&cat.board_run_in.to_uppercase(), columns.mode),
            Style::new().fg(t.overlay1).bold(),
        ));
    }
    if columns.worker > 0 {
        spans.push(Span::styled(
            pad(&cat.pane.to_uppercase(), columns.worker),
            Style::new().fg(t.overlay1).bold(),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_empty(f: &mut RenderTarget, area: Rect, cat: &Catalog, t: &Theme) {
    if area.height == 0 {
        return;
    }
    let key = |key: &'static str| {
        Span::styled(
            format!(" {key} "),
            Style::new().fg(t.base).bg(t.accent).bold(),
        )
    };
    let text = |value: String| Span::styled(value, Style::new().fg(t.subtext0));
    let mut lines = vec![
        Line::from(Span::styled(
            format!("  {}", cat.board_empty),
            Style::new().fg(t.text).bold(),
        )),
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            key("a"),
            text(format!(" {}  ·  ", cat.act_new)),
            key("s"),
            text(format!(" {}  ·  ", cat.board_start)),
            key("d"),
            text(format!(" {}  ·  ", cat.task_done)),
            key("m"),
            text(format!(" {}", cat.act_merge)),
        ]),
    ];
    if area.height >= 5 {
        lines.extend([
            Line::from(""),
            Line::from(Span::styled(
                "  luvus task add \"…\" --paths src/x/** --gate \"cargo test\"",
                Style::new().fg(t.overlay0),
            )),
        ]);
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// Explain the orchestration lifecycle in the detail column while the board is
/// empty. This uses only terminal text and existing localized catalog labels,
/// stays out of narrow layouts, and disappears as soon as a selected task can
/// use the column for real details.
fn draw_flow(
    f: &mut RenderTarget,
    area: Rect,
    mode: crate::orch::TaskWorkerMode,
    cat: &Catalog,
    t: &Theme,
) -> Vec<(crate::app::OrchHit, Rect)> {
    if area.height < 3 || area.width < 12 {
        return Vec::new();
    }
    let block = super::dashboard_block(cat.sec_flow.to_uppercase(), t, false);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 {
        return Vec::new();
    }

    let worktree_label = cat.board_worktree.to_uppercase();
    let workspace_label = cat.board_workspace.to_uppercase();
    let worktree_w = (super::display_width(&worktree_label) + 2) as u16;
    let workspace_w = (super::display_width(&workspace_label) + 2) as u16;
    let worktree_rect = Rect::new(inner.x, inner.y, worktree_w.min(inner.width), 1);
    let workspace_rect = Rect::new(
        worktree_rect.right().saturating_add(1),
        inner.y,
        workspace_w.min(inner.right().saturating_sub(worktree_rect.right() + 1)),
        1,
    );
    let mode_tab = |label: String, selected: bool| {
        Span::styled(
            format!(" {label} "),
            if selected {
                Style::new().fg(t.base).bg(t.accent).bold()
            } else {
                Style::new().fg(t.subtext0)
            },
        )
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            mode_tab(
                worktree_label,
                mode == crate::orch::TaskWorkerMode::Worktree,
            ),
            Span::raw(" "),
            mode_tab(
                workspace_label,
                mode == crate::orch::TaskWorkerMode::Workspace,
            ),
        ])),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let hits = vec![
        (
            crate::app::OrchHit::FlowMode(crate::orch::TaskWorkerMode::Worktree),
            worktree_rect,
        ),
        (
            crate::app::OrchHit::FlowMode(crate::orch::TaskWorkerMode::Workspace),
            workspace_rect,
        ),
    ];
    let inner = Rect::new(
        inner.x,
        inner.y.saturating_add(1),
        inner.width,
        inner.height.saturating_sub(1),
    );
    if mode == crate::orch::TaskWorkerMode::Workspace {
        let border = Style::new().fg(t.overlay0);
        let connector = Style::new().fg(t.overlay1);
        let accent = Style::new().fg(t.accent).bold();
        let text = Style::new().fg(t.text).bold();
        let muted = Style::new().fg(t.subtext0);
        let width = inner.width as usize;
        let mut lines = if width >= 41 && inner.height >= 23 {
            const GRAPH_W: usize = 41;
            const NODE_W: usize = 19;
            const NODE_INNER: usize = NODE_W - 2;
            const NODE_X: usize = (GRAPH_W - NODE_W) / 2;
            const LANE_W: usize = 17;
            const LANE_INNER: usize = LANE_W - 2;
            const LANE_X: usize = 1;
            const LANE_GAP: usize = 5;
            const AXIS: usize = GRAPH_W / 2;
            const LEFT_AXIS: usize = LANE_X + LANE_W / 2;
            const RIGHT_AXIS: usize = LANE_X + LANE_W + LANE_GAP + LANE_W / 2;

            let offset = width.saturating_sub(GRAPH_W) / 2;
            let prefix = |x: usize| Span::raw(" ".repeat(offset + x));
            let node_border = |top: bool| {
                let (left, right) = if top { ('┌', '┐') } else { ('└', '┘') };
                Line::from(vec![
                    prefix(NODE_X),
                    Span::styled(format!("{left}{}{right}", "─".repeat(NODE_INNER)), border),
                ])
            };
            let node_body = |label: &str, style: Style| {
                Line::from(vec![
                    prefix(NODE_X),
                    Span::styled("│", border),
                    Span::styled(center_fit(&label.to_uppercase(), NODE_INNER), style),
                    Span::styled("│", border),
                ])
            };
            let lanes_border = |top: bool| {
                let (left, right) = if top { ('┌', '┐') } else { ('└', '┘') };
                let one = format!("{left}{}{right}", "─".repeat(LANE_INNER));
                Line::from(vec![
                    prefix(LANE_X),
                    Span::styled(one.clone(), border),
                    Span::raw(" ".repeat(LANE_GAP)),
                    Span::styled(one, border),
                ])
            };
            let lanes_body = |left: &str, right: &str, style: Style| {
                Line::from(vec![
                    prefix(LANE_X),
                    Span::styled("│", border),
                    Span::styled(center_fit(&left.to_uppercase(), LANE_INNER), style),
                    Span::styled("│", border),
                    Span::raw(" ".repeat(LANE_GAP)),
                    Span::styled("│", border),
                    Span::styled(center_fit(&right.to_uppercase(), LANE_INNER), style),
                    Span::styled("│", border),
                ])
            };
            let axis = |symbol: &'static str, label: String, style: Style| {
                Line::from(vec![
                    prefix(AXIS),
                    Span::styled(symbol, accent),
                    Span::styled(format!("  {label}"), style),
                ])
            };
            let lease_label = format!("  {}", cat.board_lease);
            let lease_gap =
                RIGHT_AXIS.saturating_sub(LEFT_AXIS + 1 + super::display_width(&lease_label));
            let fail_label = format!("↺ {}  ", cat.task_failed);
            let fail_x = NODE_X.saturating_sub(super::display_width(&fail_label));

            vec![
                node_border(true),
                node_body(cat.board_task_queue, text),
                node_body("t1   t2   t3", muted),
                node_border(false),
                axis("│", cat.act_ready.to_string(), connector),
                Line::from(vec![
                    prefix(LEFT_AXIS),
                    Span::styled(
                        format!(
                            "┌{}┴{}┐",
                            "─".repeat(AXIS - LEFT_AXIS - 1),
                            "─".repeat(RIGHT_AXIS - AXIS - 1)
                        ),
                        border,
                    ),
                ]),
                Line::from(vec![
                    prefix(LEFT_AXIS),
                    Span::styled("▼", accent),
                    Span::raw(" ".repeat(RIGHT_AXIS - LEFT_AXIS - 1)),
                    Span::styled("▼", accent),
                ]),
                lanes_border(true),
                lanes_body(
                    &format!("{} A", cat.board_agent),
                    &format!("{} B", cat.board_agent),
                    text,
                ),
                lanes_body(
                    &format!("{} A", cat.act_tab),
                    &format!("{} B", cat.act_tab),
                    muted,
                ),
                lanes_border(false),
                Line::from(vec![
                    prefix(LEFT_AXIS),
                    Span::styled("│", border),
                    Span::styled(lease_label.clone(), connector),
                    Span::raw(" ".repeat(lease_gap)),
                    Span::styled("│", border),
                    Span::styled(lease_label, connector),
                ]),
                Line::from(vec![
                    prefix(LEFT_AXIS),
                    Span::styled(
                        format!(
                            "└{}┬{}┘",
                            "─".repeat(AXIS - LEFT_AXIS - 1),
                            "─".repeat(RIGHT_AXIS - AXIS - 1)
                        ),
                        border,
                    ),
                ]),
                node_border(true),
                node_body(cat.board_shared_checkout, Style::new().fg(t.amber).bold()),
                node_border(false),
                axis("│", String::new(), connector),
                node_border(true),
                Line::from(vec![
                    prefix(fail_x),
                    Span::styled(fail_label, Style::new().fg(t.coral)),
                    Span::styled("│", border),
                    Span::styled(
                        center_fit(&cat.board_quality_gate.to_uppercase(), NODE_INNER),
                        text,
                    ),
                    Span::styled("│", border),
                ]),
                node_border(false),
                axis("│", cat.board_pass.to_string(), connector),
                axis("▼", String::new(), connector),
                Line::from(vec![
                    prefix(AXIS.saturating_sub(1)),
                    Span::styled("◆ ", Style::new().fg(t.green).bold()),
                    Span::styled(
                        cat.task_done.to_uppercase(),
                        Style::new().fg(t.green).bold(),
                    ),
                ]),
            ]
        } else {
            let tree_w = 31.min(width);
            let offset = width.saturating_sub(tree_w) / 2;
            let prefix = |depth: usize| Span::raw(" ".repeat(offset + depth));
            vec![
                Line::from(vec![
                    prefix(0),
                    Span::styled("┌─ ", border),
                    Span::styled(cat.board_task_queue.to_uppercase(), text),
                ]),
                Line::from(vec![
                    prefix(0),
                    Span::styled("├────▶ ", border),
                    Span::styled(format!("{} A", cat.board_agent), accent),
                ]),
                Line::from(vec![
                    prefix(7),
                    Span::styled("└─ ", border),
                    Span::styled(format!("{} A · {}", cat.act_tab, cat.board_lease), muted),
                ]),
                Line::from(vec![
                    prefix(0),
                    Span::styled("└────▶ ", border),
                    Span::styled(format!("{} B", cat.board_agent), accent),
                ]),
                Line::from(vec![
                    prefix(7),
                    Span::styled("└─ ", border),
                    Span::styled(format!("{} B · {}", cat.act_tab, cat.board_lease), muted),
                ]),
                Line::from(vec![
                    prefix(10),
                    Span::styled("└─ ", border),
                    Span::styled(
                        cat.board_shared_checkout.to_uppercase(),
                        Style::new().fg(t.amber).bold(),
                    ),
                ]),
                Line::from(vec![
                    prefix(13),
                    Span::styled("└─ ", border),
                    Span::styled(cat.board_quality_gate.to_uppercase(), text),
                    Span::styled(format!("  ↺ {}", cat.task_failed), Style::new().fg(t.coral)),
                ]),
                Line::from(vec![
                    prefix(16),
                    Span::styled("└─ ◆ ", border),
                    Span::styled(
                        cat.task_done.to_uppercase(),
                        Style::new().fg(t.green).bold(),
                    ),
                ]),
            ]
        };
        lines.truncate(inner.height as usize);
        let content = Rect::new(
            inner.x,
            inner.y + inner.height.saturating_sub(lines.len() as u16) / 2,
            inner.width,
            lines.len() as u16,
        );
        f.render_widget(Paragraph::new(lines), content);
        return hits;
    }

    let border = Style::new().fg(t.overlay0);
    let connector = Style::new().fg(t.overlay1);
    let accent = Style::new().fg(t.accent).bold();
    let text = Style::new().fg(t.text).bold();
    let muted = Style::new().fg(t.subtext0);
    let width = inner.width as usize;

    // The full graph has two parallel worker lanes. Its fixed geometry keeps
    // every branch and join aligned while localized labels are fitted inside
    // the boxes. Smaller detail columns get the same branching model in a
    // compact tree rather than falling back to a linear checklist.
    let mut lines = if width >= 41 && inner.height >= 23 {
        const GRAPH_W: usize = 41;
        const NODE_W: usize = 19;
        const NODE_INNER: usize = NODE_W - 2;
        const NODE_X: usize = (GRAPH_W - NODE_W) / 2;
        const LANE_W: usize = 17;
        const LANE_INNER: usize = LANE_W - 2;
        const LANE_X: usize = 1;
        const LANE_GAP: usize = 5;
        const AXIS: usize = GRAPH_W / 2;
        const LEFT_AXIS: usize = LANE_X + LANE_W / 2;
        const RIGHT_AXIS: usize = LANE_X + LANE_W + LANE_GAP + LANE_W / 2;

        let offset = width.saturating_sub(GRAPH_W) / 2;
        let prefix = |x: usize| Span::raw(" ".repeat(offset + x));
        let node_border = |top: bool| {
            let (left, right) = if top { ('┌', '┐') } else { ('└', '┘') };
            Line::from(vec![
                prefix(NODE_X),
                Span::styled(format!("{left}{}{right}", "─".repeat(NODE_INNER)), border),
            ])
        };
        let node_body = |label: &str, style: Style| {
            Line::from(vec![
                prefix(NODE_X),
                Span::styled("│", border),
                Span::styled(center_fit(&label.to_uppercase(), NODE_INNER), style),
                Span::styled("│", border),
            ])
        };
        let lanes_border = |top: bool| {
            let (left, right) = if top { ('┌', '┐') } else { ('└', '┘') };
            let one = format!("{left}{}{right}", "─".repeat(LANE_INNER));
            Line::from(vec![
                prefix(LANE_X),
                Span::styled(one.clone(), border),
                Span::raw(" ".repeat(LANE_GAP)),
                Span::styled(one, border),
            ])
        };
        let lanes_body = |left: &str, right: &str, style: Style| {
            Line::from(vec![
                prefix(LANE_X),
                Span::styled("│", border),
                Span::styled(center_fit(&left.to_uppercase(), LANE_INNER), style),
                Span::styled("│", border),
                Span::raw(" ".repeat(LANE_GAP)),
                Span::styled("│", border),
                Span::styled(center_fit(&right.to_uppercase(), LANE_INNER), style),
                Span::styled("│", border),
            ])
        };
        let axis = |symbol: &'static str, label: String, style: Style| {
            Line::from(vec![
                prefix(AXIS),
                Span::styled(symbol, accent),
                Span::styled(format!("  {label}"), style),
            ])
        };
        let lease_label = format!("  {}", cat.board_lease);
        let lease_gap =
            RIGHT_AXIS.saturating_sub(LEFT_AXIS + 1 + super::display_width(&lease_label));
        let fail_label = format!("↺ {}  ", cat.task_failed);
        let fail_x = NODE_X.saturating_sub(super::display_width(&fail_label));

        vec![
            node_border(true),
            node_body(cat.board_task_queue, text),
            node_body("t1   t2   t3", muted),
            node_border(false),
            axis("│", cat.act_ready.to_string(), connector),
            Line::from(vec![
                prefix(LEFT_AXIS),
                Span::styled(
                    format!(
                        "┌{}┴{}┐",
                        "─".repeat(AXIS - LEFT_AXIS - 1),
                        "─".repeat(RIGHT_AXIS - AXIS - 1)
                    ),
                    border,
                ),
            ]),
            Line::from(vec![
                prefix(LEFT_AXIS),
                Span::styled("▼", accent),
                Span::raw(" ".repeat(RIGHT_AXIS - LEFT_AXIS - 1)),
                Span::styled("▼", accent),
            ]),
            lanes_border(true),
            lanes_body(
                &format!("{} A", cat.board_agent),
                &format!("{} B", cat.board_agent),
                text,
            ),
            lanes_body(
                &format!("{} A", cat.board_worktree),
                &format!("{} B", cat.board_worktree),
                muted,
            ),
            lanes_border(false),
            Line::from(vec![
                prefix(LEFT_AXIS),
                Span::styled("│", border),
                Span::styled(lease_label.clone(), connector),
                Span::raw(" ".repeat(lease_gap)),
                Span::styled("│", border),
                Span::styled(lease_label, connector),
            ]),
            Line::from(vec![
                prefix(LEFT_AXIS),
                Span::styled(
                    format!(
                        "└{}┬{}┘",
                        "─".repeat(AXIS - LEFT_AXIS - 1),
                        "─".repeat(RIGHT_AXIS - AXIS - 1)
                    ),
                    border,
                ),
            ]),
            node_border(true),
            Line::from(vec![
                prefix(fail_x),
                Span::styled(fail_label, Style::new().fg(t.coral)),
                Span::styled("│", border),
                Span::styled(
                    center_fit(&cat.board_quality_gate.to_uppercase(), NODE_INNER),
                    text,
                ),
                Span::styled("│", border),
            ]),
            node_border(false),
            axis("│", cat.board_pass.to_string(), connector),
            axis("▼", String::new(), connector),
            node_border(true),
            node_body(cat.act_merge, text),
            node_border(false),
            axis("▼", String::new(), connector),
            Line::from(vec![
                prefix(AXIS.saturating_sub(1)),
                Span::styled("◆ ", Style::new().fg(t.green).bold()),
                Span::styled(
                    cat.task_merged.to_uppercase(),
                    Style::new().fg(t.green).bold(),
                ),
            ]),
        ]
    } else {
        let tree_w = 25.min(width);
        let offset = width.saturating_sub(tree_w) / 2;
        let prefix = |depth: usize| Span::raw(" ".repeat(offset + depth));
        vec![
            Line::from(vec![
                prefix(0),
                Span::styled("┌─ ", border),
                Span::styled(cat.board_task_queue.to_uppercase(), text),
            ]),
            Line::from(vec![
                prefix(0),
                Span::styled("├────▶ ", border),
                Span::styled(format!("{} A", cat.board_agent), accent),
            ]),
            Line::from(vec![
                prefix(0),
                Span::styled("└────▶ ", border),
                Span::styled(format!("{} B", cat.board_agent), accent),
            ]),
            Line::from(vec![
                prefix(7),
                Span::styled("└─ ", border),
                Span::styled(cat.board_worktree.to_uppercase(), muted),
            ]),
            Line::from(vec![
                prefix(10),
                Span::styled("└─ ", border),
                Span::styled(cat.board_quality_gate.to_uppercase(), text),
                Span::styled(format!("  ↺ {}", cat.task_failed), Style::new().fg(t.coral)),
            ]),
            Line::from(vec![
                prefix(13),
                Span::styled("└─ ", border),
                Span::styled(cat.act_merge.to_uppercase(), text),
            ]),
            Line::from(vec![
                prefix(16),
                Span::styled("└─ ", border),
                Span::styled(
                    format!("◆ {}", cat.task_merged.to_uppercase()),
                    Style::new().fg(t.green).bold(),
                ),
            ]),
        ]
    };
    lines.truncate(inner.height as usize);
    let content = Rect::new(
        inner.x,
        inner.y + inner.height.saturating_sub(lines.len() as u16) / 2,
        inner.width,
        lines.len() as u16,
    );
    f.render_widget(Paragraph::new(lines), content);
    hits
}

fn center_fit(value: &str, width: usize) -> String {
    let fitted = pad(value, width);
    let value = fitted.trim_end_matches(' ');
    let used = super::display_width(value);
    let left = width.saturating_sub(used) / 2;
    let right = width.saturating_sub(used + left);
    format!("{}{value}{}", " ".repeat(left), " ".repeat(right))
}

fn draw_leases(f: &mut RenderTarget, area: Rect, orch: &OrchState, cat: &Catalog, t: &Theme) {
    if area.height < 2 || area.width < 4 {
        return;
    }
    let block = super::dashboard_block(
        format!("{} {:02}", cat.board_leases, orch.leases.len()),
        t,
        false,
    );
    let inner = block.inner(area);
    f.render_widget(block, area);
    if orch.leases.is_empty() {
        if inner.height > 0 {
            f.render_widget(
                Paragraph::new(Span::styled(
                    format!(" {}", cat.board_none),
                    Style::new().fg(t.overlay0),
                )),
                Rect::new(inner.x, inner.y, inner.width, 1),
            );
        }
        return;
    }
    for (row, lease) in orch.leases.iter().take(inner.height as usize).enumerate() {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!(" {:<4}", lease.id), Style::new().fg(t.subtext0)),
                Span::styled(format!("{}  ", lease.task), Style::new().fg(t.mint)),
                Span::styled(lease.paths.join(" "), Style::new().fg(t.text)),
            ])),
            Rect::new(inner.x, inner.y + row as u16, inner.width, 1),
        );
    }
}

fn draw_summary(f: &mut RenderTarget, area: Rect, task: &Task, cat: &Catalog, t: &Theme) {
    let block = super::dashboard_block(
        format!("{} · {}", cat.board_selected_task.to_uppercase(), task.id),
        t,
        false,
    );
    let inner = block.inner(area);
    f.render_widget(block, area);
    let sc = status_color(task.status, t);
    let mut lines = vec![
        Line::from(vec![
            Span::styled(format!(" {} ", task.id), Style::new().fg(t.subtext1).bold()),
            Span::styled(status_label(task.status, cat), Style::new().fg(sc)),
        ]),
        Line::from(Span::styled(
            format!(" {}", task.title),
            Style::new().fg(t.text).bold(),
        )),
        Line::from(""),
    ];
    let mut add = |label: &str, value: String| {
        if !value.is_empty() {
            lines.push(Line::from(vec![
                Span::styled(format!(" {label:<9}"), Style::new().fg(t.subtext0)),
                Span::styled(value, Style::new().fg(t.text)),
            ]));
        }
    };
    add(
        "pane",
        task.assignee
            .map(|pane| pane.to_string())
            .unwrap_or_default(),
    );
    add("branch", task.branch.clone().unwrap_or_default());
    add("worktree", task.worktree.clone().unwrap_or_default());
    add(cat.board_f_paths, task.paths.join(" "));
    add(cat.board_f_deps, task.deps.join(" "));
    add(cat.board_f_gate, task.gate.clone().unwrap_or_default());
    if let Some(output) = task.outputs.last() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(" {}", cat.board_outputs),
            Style::new().fg(t.subtext1).bold(),
        )));
        lines.extend(output.lines().take(3).map(|line| {
            Line::from(Span::styled(
                format!("  {line}"),
                Style::new().fg(t.subtext0),
            ))
        }));
    }
    if let Some(note) = task.notes.last() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(" {}", cat.board_notes),
            Style::new().fg(t.subtext1).bold(),
        )));
        lines.extend(note.lines().take(2).map(|line| {
            Line::from(Span::styled(
                format!("  {line}"),
                Style::new().fg(t.subtext0),
            ))
        }));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

fn fill_bg(f: &mut RenderTarget, rect: Rect, color: Color) {
    let buf = f.buffer_mut();
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            buf[(x, y)].set_bg(color);
        }
    }
}

/// The in-TUI new-task form (ORCH-7): a small modal with Title/Paths/Deps/Gate
/// fields; the active field is highlighted with a cursor. Drawn last, over a
/// dimmed backdrop, like the other modals.
pub(super) fn draw_form(
    f: &mut RenderTarget,
    area: Rect,
    form: &OrchForm,
    cat: &Catalog,
    t: &Theme,
) -> Vec<(crate::app::OrchHit, Rect)> {
    let mut hits = Vec::with_capacity(7);
    dim_backdrop(f, area, t);
    let w = area.width.saturating_sub(6).clamp(44, 76).min(area.width);
    let modal = centered_rect(area, w, 10);
    f.render_widget(Clear, modal);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(t.border_focus).bg(t.surface0))
        .style(Style::new().bg(t.surface0));
    let inner = block.inner(modal);
    f.render_widget(block, modal);

    f.render_widget(
        Paragraph::new(Span::styled(
            format!(" {}", cat.board_new_task),
            Style::new().fg(t.text).bold(),
        )),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let vals = form.values();
    let labels = [
        cat.board_f_title,
        cat.board_f_paths,
        cat.board_f_deps,
        cat.board_f_gate,
    ];
    let hints = [
        cat.board_h_title,
        cat.board_h_paths,
        cat.board_h_deps,
        cat.board_h_gate,
    ];
    for (i, label) in labels.iter().enumerate() {
        let active = i == form.field;
        let label_style = if active {
            Style::new().fg(t.accent).bold()
        } else {
            Style::new().fg(t.subtext0)
        };
        // A subtle hint of what each field expects, shown when it's empty.
        let body = if vals[i].is_empty() && !active {
            Span::styled(hints[i], Style::new().fg(t.overlay0))
        } else {
            Span::styled(vals[i].clone(), Style::new().fg(t.text))
        };
        let field_rect = Rect::new(inner.x, inner.y + 2 + i as u16, inner.width, 1);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!(" {label:<6}: "), label_style),
                body,
                Span::styled(if active { "▏" } else { "" }, Style::new().fg(t.accent)),
            ])),
            field_rect,
        );
        hits.push((crate::app::OrchHit::FormField(i), field_rect));
    }

    let bottom = inner.bottom().saturating_sub(1);
    if let Some(e) = &form.error {
        f.render_widget(
            Paragraph::new(Span::styled(format!(" {e}"), Style::new().fg(t.coral))),
            Rect::new(inner.x, bottom, inner.width, 1),
        );
    } else {
        f.render_widget(
            Paragraph::new(super::hint_line(
                &[
                    ("⏎", cat.act_create),
                    ("⇥", cat.board_next_field),
                    ("esc", cat.act_cancel),
                ],
                t,
            )),
            Rect::new(inner.x, bottom, inner.width, 1),
        );
        let left_w = inner.width / 2;
        hits.push((
            crate::app::OrchHit::FormCreate,
            Rect::new(inner.x, bottom, left_w, 1),
        ));
        hits.push((
            crate::app::OrchHit::FormCancel,
            Rect::new(
                inner.x + left_w,
                bottom,
                inner.width.saturating_sub(left_w),
                1,
            ),
        ));
    }
    // Actionable controls are intentionally inserted first. The modal surface
    // is the fallback hit target, which keeps clicks inside it from behaving
    // like backdrop clicks.
    hits.push((crate::app::OrchHit::FormModal, modal));
    hits
}

fn centered_rect(area: Rect, w: u16, h: u16) -> Rect {
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w.min(area.width), h.min(area.height))
}

fn dim_backdrop(f: &mut RenderTarget, area: Rect, t: &Theme) {
    let buf = f.buffer_mut();
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            buf[(x, y)].set_fg(t.overlay0).set_bg(t.crust);
        }
    }
}

struct TaskRow<'a> {
    line: Line<'a>,
    worker_col: Option<usize>,
}

fn task_line<'a>(
    task: &'a Task,
    columns: &TaskColumns,
    selected: bool,
    cat: &Catalog,
    t: &Theme,
) -> TaskRow<'a> {
    let sc = status_color(task.status, t);
    let deps = if task.deps.is_empty() {
        String::new()
    } else {
        task.deps.join(",")
    };
    let mut spans = vec![
        Span::styled(
            if selected { "▌ " } else { "  " },
            Style::new().fg(t.accent),
        ),
        Span::styled(
            pad(&task.id, columns.id),
            Style::new().fg(t.subtext1).bold(),
        ),
        Span::styled(
            pad(
                &format!(
                    "{} {}",
                    status_dot(task.status),
                    status_label(task.status, cat)
                ),
                columns.status,
            ),
            Style::new().fg(sc),
        ),
        Span::styled(pad(&task.title, columns.title), Style::new().fg(t.text)),
    ];
    if columns.deps > 0 {
        spans.push(Span::styled(
            pad(&deps, columns.deps),
            Style::new().fg(t.overlay1),
        ));
    }
    if columns.mode > 0 {
        let mode = match task.worker_mode.or_else(|| {
            task.worktree
                .as_ref()
                .map(|_| crate::orch::TaskWorkerMode::Worktree)
        }) {
            Some(crate::orch::TaskWorkerMode::Worktree) => cat.board_worktree,
            Some(crate::orch::TaskWorkerMode::Workspace) => cat.board_workspace,
            None => "",
        };
        spans.push(Span::styled(
            pad(mode, columns.mode),
            Style::new().fg(t.subtext0),
        ));
    }
    if columns.worker > 0 {
        spans.extend(worker_spans(task, columns.worker, cat, t));
    }
    TaskRow {
        line: Line::from(spans),
        worker_col: columns.worker_col(),
    }
}

fn worker_spans<'a>(task: &'a Task, width: usize, cat: &Catalog, t: &Theme) -> Vec<Span<'a>> {
    match task.assignee {
        Some(pane) => {
            let pane = (format!("pane {pane}"), t.subtext0);
            let branch = task
                .branch
                .as_ref()
                .map(|branch| (branch.clone(), t.subtext0));
            let mut candidates = vec![[Some(pane.clone()), branch.clone()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()];
            if let Some(branch) = branch {
                candidates.push(vec![branch]);
            }
            candidates.push(vec![pane]);
            fit_worker_parts(candidates, width, t)
        }
        None if task.worktree.is_some() || task.workspace_worker.is_some() => {
            let no_pane = (cat.board_no_pane.to_string(), t.overlay1);
            let branch = task
                .branch
                .as_ref()
                .map(|branch| (branch.clone(), t.subtext0));
            let mut candidates = vec![[Some(no_pane.clone()), branch.clone()]
                .into_iter()
                .flatten()
                .collect()];
            if let Some(branch) = branch {
                candidates.push(vec![branch]);
            }
            candidates.push(vec![no_pane]);
            fit_worker_parts(candidates, width, t)
        }
        None => vec![Span::raw(" ".repeat(width))],
    }
}

fn fit_worker_parts<'a>(
    candidates: Vec<Vec<(String, Color)>>,
    width: usize,
    t: &Theme,
) -> Vec<Span<'a>> {
    let parts = candidates
        .into_iter()
        .find(|parts| worker_parts_width(parts) <= width)
        .unwrap_or_default();
    let mut spans = Vec::new();
    for (index, (text, color)) in parts.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", Style::new().fg(t.overlay0)));
        }
        spans.push(Span::styled(text, Style::new().fg(color)));
    }
    let used = spans
        .iter()
        .map(|span| super::display_width(&span.content))
        .sum::<usize>();
    spans.push(Span::raw(" ".repeat(width.saturating_sub(used))));
    spans
}

fn worker_parts_width(parts: &[(String, Color)]) -> usize {
    parts
        .iter()
        .map(|(text, _)| super::display_width(text))
        .sum::<usize>()
        + parts.len().saturating_sub(1) * 3
}

/// The two-step **start-worker picker** (board `s`): choose worktree/workspace,
/// then the agent. `⏎` confirms the current step; `esc` cancels.
pub(super) fn draw_start(
    f: &mut RenderTarget,
    area: Rect,
    start: &crate::app::OrchStart,
    cat: &Catalog,
    t: &Theme,
) -> Vec<(crate::app::OrchHit, Rect)> {
    let mut hits = Vec::with_capacity(crate::app::agent_choices().len() + 4);
    dim_backdrop(f, area, t);
    let choices = crate::app::agent_choices();
    let mode_step = start.step == crate::app::OrchStartStep::Mode;
    let requested_h = if mode_step {
        8
    } else {
        (choices.len() as u16) + 4
    };
    let h = requested_h.min(area.height.saturating_sub(2).max(4));
    let modal = centered_rect(area, 44.min(area.width), h);
    f.render_widget(Clear, modal);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(t.border_focus).bg(t.surface0))
        .style(Style::new().bg(t.surface0));
    let inner = block.inner(modal);
    f.render_widget(block, modal);

    f.render_widget(
        Paragraph::new(Span::styled(
            if mode_step {
                format!(" {} — {}  1/2", cat.board_start_with, start.task)
            } else {
                format!(
                    " {} — {}  2/2 · {}/{}",
                    cat.board_start_with,
                    start.task,
                    start.cursor + 1,
                    choices.len()
                )
            },
            Style::new().fg(t.text).bold(),
        )),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    if mode_step {
        f.render_widget(
            Paragraph::new(Span::styled(
                format!(" {}", cat.board_run_in.to_uppercase()),
                Style::new().fg(t.overlay1).bold(),
            )),
            Rect::new(inner.x, inner.y + 1, inner.width, 1),
        );
        for (i, (mode, label)) in [
            (crate::orch::TaskWorkerMode::Worktree, cat.board_worktree),
            (crate::orch::TaskWorkerMode::Workspace, cat.board_workspace),
        ]
        .into_iter()
        .enumerate()
        {
            let selected = mode == start.mode;
            let rect = Rect::new(inner.x, inner.y + 2 + i as u16, inner.width, 1);
            if selected {
                fill_bg(f, rect, t.surface1);
            }
            f.render_widget(
                Paragraph::new(Span::styled(
                    format!("  {} {}", if selected { "▸" } else { " " }, label),
                    if selected {
                        Style::new().fg(t.text).bg(t.surface1).bold()
                    } else {
                        Style::new().fg(t.subtext0)
                    },
                )),
                rect,
            );
            hits.push((crate::app::OrchHit::StartMode(mode), rect));
        }
        if start.mode == crate::orch::TaskWorkerMode::Workspace {
            let warning = if start.shared_workers == 0 {
                cat.board_shared_checkout.to_string()
            } else {
                format!(
                    "{} · {} {}",
                    cat.board_shared_checkout, start.shared_workers, cat.active
                )
            };
            f.render_widget(
                Paragraph::new(Span::styled(
                    format!("  {warning}"),
                    Style::new().fg(t.amber),
                )),
                Rect::new(inner.x, inner.y + 4, inner.width, 1),
            );
        }
        f.render_widget(
            Paragraph::new(super::hint_line(
                &[("⏎", cat.act_select), ("esc", cat.act_cancel)],
                t,
            )),
            Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
        );
    } else {
        let visible_rows = inner.height.saturating_sub(2) as usize;
        let first = start.cursor.saturating_add(1).saturating_sub(visible_rows);
        for (visible, (i, (label, cmd))) in choices
            .iter()
            .enumerate()
            .skip(first)
            .take(visible_rows)
            .enumerate()
        {
            let selected = i == start.cursor;
            let name = if cmd.is_some() {
                (*label).to_string()
            } else {
                cat.board_shell_only.to_string()
            };
            let style = if selected {
                Style::new().fg(t.text).bg(t.surface1).bold()
            } else {
                Style::new().fg(t.subtext0)
            };
            let rect = Rect::new(inner.x, inner.y + 1 + visible as u16, inner.width, 1);
            if selected {
                fill_bg(f, rect, t.surface1);
            }
            f.render_widget(
                Paragraph::new(Span::styled(
                    format!("  {} {}", if selected { "▸" } else { " " }, name),
                    style,
                )),
                rect,
            );
            hits.push((crate::app::OrchHit::StartChoice(i), rect));
        }
        f.render_widget(
            Paragraph::new(super::hint_line(
                &[
                    ("⏎", cat.board_start),
                    ("⌫", cat.act_back),
                    ("esc", cat.act_cancel),
                ],
                t,
            )),
            Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
        );
    }
    let bottom = inner.bottom().saturating_sub(1);
    let left_w = inner.width / 2;
    hits.push((
        crate::app::OrchHit::StartCommit,
        Rect::new(inner.x, bottom, left_w, 1),
    ));
    hits.push((
        crate::app::OrchHit::StartCancel,
        Rect::new(
            inner.x + left_w,
            bottom,
            inner.width.saturating_sub(left_w),
            1,
        ),
    ));
    hits
}

/// The **task detail overlay** (board `o`): everything about one task — branch,
/// worktree, paths, gate, and the captured gate output + notes (the things you
/// need when a gate fails). `j/k`/wheel scroll, `esc`/`o` close. Returns the
/// clamped scroll to write back.
pub(super) struct DetailRender {
    pub scroll: usize,
    pub hits: Vec<(crate::app::OrchHit, Rect)>,
}

pub(super) fn draw_detail(
    f: &mut RenderTarget,
    area: Rect,
    task: &Task,
    scroll: usize,
    cat: &Catalog,
    t: &Theme,
) -> DetailRender {
    dim_backdrop(f, area, t);
    let w = area.width.saturating_sub(6).clamp(44, 78).min(area.width);
    let h = area.height.saturating_sub(4).clamp(8, 24).min(area.height);
    let modal = centered_rect(area, w, h);
    f.render_widget(Clear, modal);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(t.border_focus).bg(t.surface0))
        .style(Style::new().bg(t.surface0));
    let inner = block.inner(modal);
    f.render_widget(block, modal);

    let close = Rect::new(modal.right().saturating_sub(3), modal.y, 2, 1);
    f.render_widget(
        Paragraph::new(Span::styled("×", Style::new().fg(t.subtext0).bold())),
        close,
    );

    let sc = status_color(task.status, t);
    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled(format!(" {} ", task.id), Style::new().fg(t.subtext1).bold()),
            Span::styled(status_label(task.status, cat), Style::new().fg(sc)),
            Span::styled(
                format!(
                    "  {}",
                    pad(&task.title, (inner.width as usize).saturating_sub(14))
                ),
                Style::new().fg(t.text).bold(),
            ),
        ]),
        Line::from(""),
    ];
    let kv = |k: &'static str, v: String, lines: &mut Vec<Line>| {
        if !v.is_empty() {
            lines.push(Line::from(vec![
                Span::styled(format!(" {k:<9}"), Style::new().fg(t.subtext0)),
                Span::styled(v, Style::new().fg(t.text)),
            ]));
        }
    };
    match task.worker_mode.or_else(|| {
        task.worktree
            .as_ref()
            .map(|_| crate::orch::TaskWorkerMode::Worktree)
    }) {
        Some(crate::orch::TaskWorkerMode::Worktree) => {
            kv("mode", cat.board_worktree.to_string(), &mut lines)
        }
        Some(crate::orch::TaskWorkerMode::Workspace) => {
            kv("mode", cat.board_workspace.to_string(), &mut lines);
            kv(
                "isolation",
                cat.board_shared_checkout.to_string(),
                &mut lines,
            );
        }
        None => {}
    }
    if let Some(b) = &task.branch {
        kv("branch", b.clone(), &mut lines);
    }
    if let Some(wt) = &task.worktree {
        kv("worktree", wt.clone(), &mut lines);
    }
    if let Some(binding) = &task.workspace_worker {
        kv("workspace", binding.workspace_id.clone(), &mut lines);
        kv("directory", binding.root.clone(), &mut lines);
    }
    kv(
        "pane",
        task.assignee.map(|p| p.to_string()).unwrap_or_default(),
        &mut lines,
    );
    kv(cat.board_f_paths, task.paths.join(" "), &mut lines);
    kv(cat.board_f_deps, task.deps.join(" "), &mut lines);
    kv(
        cat.board_f_gate,
        task.gate.clone().unwrap_or_default(),
        &mut lines,
    );
    if !task.outputs.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(" {}", cat.board_outputs),
            Style::new().fg(t.subtext1).bold(),
        )));
        for o in task.outputs.iter().rev().take(5).rev() {
            for l in o.lines() {
                lines.push(Line::from(Span::styled(
                    format!("  {l}"),
                    Style::new().fg(t.subtext0),
                )));
            }
        }
    }
    if !task.notes.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(" {}", cat.board_notes),
            Style::new().fg(t.subtext1).bold(),
        )));
        for n in task.notes.iter().rev().take(5).rev() {
            for l in n.lines() {
                lines.push(Line::from(Span::styled(
                    format!("  {l}"),
                    Style::new().fg(t.subtext0),
                )));
            }
        }
    }

    let body = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(1),
    );
    let vis = body.height as usize;
    let scroll = scroll.min(lines.len().saturating_sub(vis));
    f.render_widget(Paragraph::new(lines).scroll((scroll as u16, 0)), body);
    f.render_widget(
        Paragraph::new(super::hint_line(
            &[("j/k", cat.act_select), ("esc", cat.act_close)],
            t,
        )),
        Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
    );
    DetailRender {
        scroll,
        hits: vec![(crate::app::OrchHit::DetailClose, close)],
    }
}

fn fmt_count(label: &str, n: usize) -> String {
    format!("{n} {label}")
}

fn status_index(s: TaskStatus) -> usize {
    match s {
        TaskStatus::Queued => 0,
        TaskStatus::Claimed => 1,
        TaskStatus::Running => 2,
        TaskStatus::Blocked => 3,
        TaskStatus::Review => 4,
        TaskStatus::Done => 5,
        TaskStatus::Merging => 6,
        TaskStatus::Merged => 7,
        TaskStatus::Failed => 8,
    }
}

fn hline(f: &mut RenderTarget, x: u16, y: u16, w: u16, t: &Theme) {
    let buf = f.buffer_mut();
    for i in 0..w {
        buf[(x + i, y)]
            .set_symbol("─")
            .set_style(Style::new().fg(t.surface1).bg(t.mantle));
    }
}

/// Truncate then pad `s` to exactly `n` display columns.
fn pad(s: &str, n: usize) -> String {
    let w = super::display_width(s);
    if w > n {
        let mut out = String::new();
        let mut used = 0;
        for ch in s.chars() {
            let cw = super::display_width(&ch.to_string());
            if used + cw > n.saturating_sub(1) {
                break;
            }
            out.push(ch);
            used += cw;
        }
        out.push('…');
        while super::display_width(&out) < n {
            out.push(' ');
        }
        out
    } else {
        format!("{s}{}", " ".repeat(n - w))
    }
}
