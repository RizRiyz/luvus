//! The orchestration board dashboard (docs/22, ORCH-7): a header with task
//! counts, an interactive list of tasks (status dot · id · state · title · deps ·
//! assignee), the active path leases, and the new-task form. Pure ratatui,
//! localized through the i18n catalog (docs/21), and built from the same panel
//! chrome as Mission Control. Rendered from the shared `OrchState`.

use super::*;
use crate::app::OrchForm;
use crate::app::OrchView;
use crate::automation::{Automation, AutomationState, RunStatus, Trigger};
use crate::i18n::Catalog;
use crate::orch::{OrchState, Task, TaskStatus};
use ratatui::widgets::{Borders, Clear, Wrap};

/// Keep the fleet summary renderer available without spending a viewport row
/// while the task and automation tables already expose the same state.
const SHOW_FLEET_STATUS: bool = false;

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
    automation: &AutomationState,
    scroll: usize,
    cursor: usize,
    automation_cursor: usize,
    view: OrchView,
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
    // the first row, then a quiet separator.
    let action_text = format!(" + {} ", cat.board_new_task.to_uppercase());
    let action_w = (super::display_width(&action_text) as u16).min(area.width);
    let action = Rect::new(
        area.right().saturating_sub(action_w.saturating_add(1)),
        area.y,
        action_w,
        1,
    );
    let title = format!(" ◎ {}  ", cat.board_title);
    f.render_widget(
        Paragraph::new(Span::styled(title.clone(), Style::new().fg(t.text).bold())),
        Rect::new(
            area.x,
            area.y,
            area.width.saturating_sub(action_w.saturating_add(2)),
            1,
        ),
    );
    let mut view_x = area.x.saturating_add(display_width(&title) as u16);
    for (kind, label) in [
        (OrchView::Tasks, cat.board_tasks.to_uppercase()),
        (
            OrchView::Automations,
            format!("{} BETA", cat.board_automations.to_uppercase()),
        ),
    ] {
        let text = format!(" {label} ");
        let width =
            (display_width(&text) as u16).min(action.x.saturating_sub(view_x).saturating_sub(1));
        if width == 0 {
            break;
        }
        let rect = Rect::new(view_x, area.y, width, 1);
        let selected = kind == view;
        f.render_widget(
            Paragraph::new(Span::styled(
                truncate(&text, width as usize),
                Style::new()
                    .fg(if selected { t.crust } else { t.overlay1 })
                    .bg(if selected { t.accent } else { t.mantle })
                    .bold(),
            )),
            rect,
        );
        hits.push((crate::app::OrchHit::View(kind), rect));
        view_x = view_x.saturating_add(width).saturating_add(1);
    }
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
    let footer_h = u16::from(!compact && area.height >= 10);
    let summary_h = u16::from(SHOW_FLEET_STATUS && area.height >= 5);
    let summary_y = area
        .bottom()
        .saturating_sub(footer_h.saturating_add(summary_h));
    if summary_h > 0 {
        let mut counts = [0usize; 9];
        for task in &orch.tasks {
            counts[status_index(task.status)] += 1;
        }
        draw_status_summary(
            f,
            Rect::new(area.x, summary_y, area.width, 1),
            view,
            &counts,
            automation,
            cat,
            t,
        );
    }
    hline(f, area.x + 1, area.y + 1, area.width.saturating_sub(2), t);

    if view == OrchView::Automations {
        return render_automations(
            f,
            area,
            AutomationRender {
                state: automation,
                cursor: automation_cursor,
                compact,
                catalog: cat,
                theme: t,
            },
            hits,
        );
    }

    let body_y = area.y.saturating_add(2);
    let body = Rect::new(
        area.x,
        body_y,
        area.width,
        area.bottom().saturating_sub(body_y + footer_h + summary_h),
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

struct AutomationRender<'a> {
    state: &'a AutomationState,
    cursor: usize,
    compact: bool,
    catalog: &'a Catalog,
    theme: &'a Theme,
}

#[derive(Clone, Copy)]
enum AutomationDisplayStatus {
    Scheduled,
    Restoring,
    NeedsRebind,
    Running,
    Review,
    Failed,
    Paused,
    Completed,
}

#[derive(Clone, Copy)]
struct AutomationColumns {
    wide: bool,
    title: usize,
    schedule: usize,
    next: usize,
    agent: usize,
    pane: usize,
}

fn automation_columns(width: u16) -> AutomationColumns {
    if width >= 112 {
        AutomationColumns {
            wide: true,
            title: 20,
            schedule: 22,
            next: 23,
            agent: 12,
            pane: 5,
        }
    } else if width >= 97 {
        AutomationColumns {
            wide: true,
            title: 15,
            schedule: 18,
            next: 23,
            agent: 9,
            pane: 4,
        }
    } else if width >= 80 {
        AutomationColumns {
            wide: true,
            title: 13,
            schedule: 15,
            next: 21,
            agent: 0,
            pane: 0,
        }
    } else {
        AutomationColumns {
            wide: false,
            title: 23,
            schedule: 0,
            next: 20,
            agent: 0,
            pane: 0,
        }
    }
}

fn draw_status_summary(
    f: &mut RenderTarget,
    area: Rect,
    view: OrchView,
    task_counts: &[usize; 9],
    automation: &AutomationState,
    cat: &Catalog,
    t: &Theme,
) {
    let entries = if view == OrchView::Automations {
        let mut counts = [0usize; 8];
        for item in &automation.automations {
            let index = match automation_display_status(automation, item) {
                AutomationDisplayStatus::Scheduled => 0,
                AutomationDisplayStatus::Restoring => 1,
                AutomationDisplayStatus::NeedsRebind => 2,
                AutomationDisplayStatus::Running => 3,
                AutomationDisplayStatus::Review => 4,
                AutomationDisplayStatus::Failed => 5,
                AutomationDisplayStatus::Paused => 6,
                AutomationDisplayStatus::Completed => 7,
            };
            counts[index] += 1;
        }
        vec![
            (cat.automation_scheduled, counts[0], t.accent),
            (cat.automation_restoring, counts[1], t.amber),
            (cat.automation_needs_rebind, counts[2], t.coral),
            (cat.task_running, counts[3], t.mint),
            (cat.task_review, counts[4], t.amber),
            (cat.task_failed, counts[5], t.coral),
            (cat.automation_paused, counts[6], t.overlay1),
            (cat.automation_completed, counts[7], t.green),
        ]
    } else {
        vec![
            (cat.task_queued, task_counts[0], t.overlay1),
            (
                cat.task_running,
                task_counts[2] + task_counts[1] + task_counts[6],
                t.mint,
            ),
            (cat.task_blocked, task_counts[3], t.coral),
            (cat.task_done, task_counts[5] + task_counts[7], t.green),
        ]
    };
    let mut spans = vec![Span::styled(
        format!("   {}  ", cat.col_status.to_uppercase()),
        Style::new().fg(t.overlay0),
    )];
    for (index, (label, count, active_color)) in entries.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("  ·  ", Style::new().fg(t.surface1)));
        }
        spans.push(Span::styled(
            fmt_count(label, count),
            Style::new().fg(if count > 0 { active_color } else { t.overlay1 }),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_automations(
    f: &mut RenderTarget,
    area: Rect,
    panel: AutomationRender<'_>,
    mut hits: Vec<(crate::app::OrchHit, Rect)>,
) -> BoardRender {
    let AutomationRender {
        state,
        cursor,
        compact,
        catalog: cat,
        theme: t,
    } = panel;
    let footer_h = u16::from(!compact && area.height >= 10);
    let summary_h = u16::from(SHOW_FLEET_STATUS && area.height >= 5);
    let body = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(2),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2 + footer_h + summary_h),
    );
    if state.automations.is_empty() {
        let header = Rect::new(body.x, body.y, body.width, 1);
        if body.height > 0 {
            draw_automation_header(f, header, automation_columns(body.width), cat, t);
        }
        let empty = Rect::new(
            body.x,
            body.y.saturating_add(1),
            body.width,
            body.height.saturating_sub(1),
        );
        draw_automation_empty(f, empty, cat, t);
        return BoardRender { scroll: 0, hits };
    }
    let cursor = cursor.min(state.automations.len().saturating_sub(1));
    let capacity = body.height.saturating_sub(1) as usize;
    let scroll = cursor.saturating_sub(capacity.saturating_sub(1));
    let overflow = capacity > 0 && state.automations.len() > capacity;
    let table_width = body.width.saturating_sub(u16::from(overflow));
    let header = Rect::new(body.x, body.y, table_width, 1);
    let columns = automation_columns(table_width);
    if body.height > 0 {
        draw_automation_header(f, header, columns, cat, t);
    }
    for (visible, automation) in state
        .automations
        .iter()
        .skip(scroll)
        .take(capacity)
        .enumerate()
    {
        let index = scroll + visible;
        let row = Rect::new(body.x, body.y + 1 + visible as u16, table_width, 1);
        let selected = index == cursor;
        if selected {
            fill_bg(f, row, t.sel_bg);
        }
        f.render_widget(
            Paragraph::new(automation_line(
                state, automation, columns, selected, cat, t,
            )),
            row,
        );
        hits.push((crate::app::OrchHit::Automation(automation.id.clone()), row));
    }
    if overflow {
        draw_automation_scrollbar(
            f,
            Rect::new(
                body.right().saturating_sub(1),
                body.y + 1,
                1,
                capacity as u16,
            ),
            state.automations.len(),
            capacity,
            scroll,
            t,
        );
    }
    if footer_h > 0 {
        f.render_widget(
            Paragraph::new(super::hint_line(
                &[
                    ("↑↓", cat.board_move_field),
                    ("tab", cat.board_switch_type),
                    ("e", cat.board_automation_toggle),
                    ("r", cat.board_start),
                    ("o", cat.board_details),
                    ("D", cat.act_delete),
                    ("q", cat.act_close),
                ],
                t,
            )),
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
        );
    }
    BoardRender { scroll, hits }
}

fn automation_line<'a>(
    state: &AutomationState,
    automation: &'a Automation,
    columns: AutomationColumns,
    selected: bool,
    cat: &'a Catalog,
    t: &'a Theme,
) -> Line<'a> {
    let (status, status_color) = automation_status(state, &automation.id, cat, t);
    let next = automation
        .next_run_at
        .map(super::format_utc)
        .unwrap_or_else(|| "—".into());
    let mut spans = vec![
        Span::styled(
            if selected { "▌ " } else { "  " },
            Style::new().fg(t.accent),
        ),
        Span::styled(
            format!("{} ", pad(&automation.id, 6)),
            Style::new().fg(t.subtext1).bold(),
        ),
        Span::styled(
            format!("{} ", pad(status, 11)),
            Style::new().fg(status_color),
        ),
        Span::styled(
            format!("{} ", pad(&automation.name, columns.title)),
            Style::new().fg(t.text),
        ),
    ];
    if columns.wide {
        spans.extend([
            Span::styled(
                format!(
                    "{} ",
                    pad(&schedule_label(&automation.trigger), columns.schedule)
                ),
                Style::new().fg(t.subtext0),
            ),
            Span::styled(
                format!("{} ", pad(&next, columns.next)),
                Style::new().fg(t.overlay1),
            ),
            Span::styled(
                if columns.agent > 0 {
                    pad(&automation.task.agent_id, columns.agent)
                } else {
                    automation.task.agent_id.clone()
                },
                Style::new().fg(t.mint).bold(),
            ),
        ]);
        if columns.pane > 0 {
            spans.extend([
                Span::raw(" "),
                Span::styled(
                    automation_pane_label(automation),
                    Style::new().fg(t.amber).bold(),
                ),
            ]);
        }
    } else {
        spans.push(Span::styled(pad(&next, 20), Style::new().fg(t.overlay1)));
    }
    Line::from(spans)
}

/// A proportional position rail for long automation lists. It uses the same
/// quiet full-cell track as the mobile navigator, but is confined to the row
/// gutter so it never obscures table content or hit targets.
fn draw_automation_scrollbar(
    f: &mut RenderTarget,
    track: Rect,
    total: usize,
    capacity: usize,
    scroll: usize,
    t: &Theme,
) {
    if total <= capacity || capacity == 0 || track.height == 0 {
        return;
    }
    let len = track.height as usize;
    let thumb = (len * capacity / total).clamp(1, len);
    let span = total - capacity;
    let position = ((len - thumb) * scroll.min(span)) / span;
    for offset in 0..len {
        fill_bg(
            f,
            Rect::new(track.x, track.y + offset as u16, 1, 1),
            if offset >= position && offset < position + thumb {
                t.overlay1
            } else {
                t.surface1
            },
        );
    }
}

fn draw_automation_header(
    f: &mut RenderTarget,
    area: Rect,
    columns: AutomationColumns,
    cat: &Catalog,
    t: &Theme,
) {
    let style = Style::new().fg(t.overlay1).bold();
    if columns.pane > 0 {
        let line = format!(
            "  {} {} {} {} {} {} {}",
            pad("ID", 6),
            pad(&cat.col_status.to_uppercase(), 11),
            pad(&cat.col_title.to_uppercase(), columns.title),
            pad(&cat.board_f_schedule.to_uppercase(), columns.schedule),
            pad(&cat.col_next_utc.to_uppercase(), columns.next),
            pad(&cat.board_agent.to_uppercase(), columns.agent),
            cat.pane.to_uppercase(),
        );
        f.render_widget(Paragraph::new(Span::styled(line, style)), area);
    } else if columns.wide {
        let line = format!(
            "  {} {} {} {} {} {}",
            pad("ID", 6),
            pad(&cat.col_status.to_uppercase(), 11),
            pad(&cat.col_title.to_uppercase(), columns.title),
            pad(&cat.board_f_schedule.to_uppercase(), columns.schedule),
            pad(&cat.col_next_utc.to_uppercase(), columns.next),
            cat.board_agent.to_uppercase(),
        );
        f.render_widget(Paragraph::new(Span::styled(line, style)), area);
    } else {
        let line = format!(
            "  {} {} {} {}",
            pad("ID", 6),
            pad(&cat.col_status.to_uppercase(), 11),
            pad(&cat.col_title.to_uppercase(), columns.title),
            cat.col_next_utc.to_uppercase(),
        );
        f.render_widget(Paragraph::new(Span::styled(line, style)), area);
    }
}

fn automation_pane_label(automation: &Automation) -> String {
    match &automation.target {
        crate::automation::AutomationTarget::ActiveAgent { pane_id, .. } => {
            format!("p{pane_id}")
        }
        crate::automation::AutomationTarget::NewWorker => "—".into(),
    }
}

fn automation_status<'a>(
    state: &AutomationState,
    id: &str,
    cat: &'a Catalog,
    t: &'a Theme,
) -> (&'a str, Color) {
    let Some(item) = state.automation(id) else {
        return (cat.automation_paused, t.overlay1);
    };
    match automation_display_status(state, item) {
        AutomationDisplayStatus::Scheduled => (cat.automation_scheduled, t.accent),
        AutomationDisplayStatus::Restoring => (cat.automation_restoring, t.amber),
        AutomationDisplayStatus::NeedsRebind => (cat.automation_needs_rebind, t.coral),
        AutomationDisplayStatus::Running => (cat.task_running, t.mint),
        AutomationDisplayStatus::Review => (cat.task_review, t.amber),
        AutomationDisplayStatus::Failed => (cat.task_failed, t.coral),
        AutomationDisplayStatus::Paused => (cat.automation_paused, t.overlay1),
        AutomationDisplayStatus::Completed => (cat.automation_completed, t.green),
    }
}

fn automation_display_status(
    state: &AutomationState,
    automation: &Automation,
) -> AutomationDisplayStatus {
    if let Some(target_state) = state.active_target_states.get(&automation.id) {
        match target_state {
            crate::automation::ActiveTargetState::Restoring => {
                return AutomationDisplayStatus::Restoring;
            }
            crate::automation::ActiveTargetState::NeedsRebind => {
                return AutomationDisplayStatus::NeedsRebind;
            }
            crate::automation::ActiveTargetState::Bound => {}
        }
    }
    match state.latest_run(&automation.id).map(|run| run.status) {
        Some(RunStatus::Pending | RunStatus::Starting | RunStatus::Running) => {
            AutomationDisplayStatus::Running
        }
        Some(RunStatus::Review) => AutomationDisplayStatus::Review,
        Some(RunStatus::Failed) => AutomationDisplayStatus::Failed,
        _ if automation.enabled => AutomationDisplayStatus::Scheduled,
        _ if matches!(automation.trigger, Trigger::Once { .. }) => {
            AutomationDisplayStatus::Completed
        }
        _ => AutomationDisplayStatus::Paused,
    }
}

fn schedule_label(trigger: &Trigger) -> String {
    match trigger {
        Trigger::Once { at_utc } => format!("once {}", super::format_utc(*at_utc)),
        Trigger::Interval { every_seconds, .. } => format!("every {every_seconds}s"),
        Trigger::Daily { second_of_day, .. } => {
            format!("daily {}", wall_time(*second_of_day))
        }
        Trigger::Weekly {
            weekdays,
            second_of_day,
            ..
        } => {
            let days = weekdays
                .iter()
                .map(u8::to_string)
                .collect::<Vec<_>>()
                .join(",");
            format!("weekly {days} {}", wall_time(*second_of_day))
        }
    }
}

fn wall_time(seconds: u32) -> String {
    format!("{:02}:{:02}", seconds / 3600, (seconds % 3600) / 60)
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
            cat.board_empty,
            Style::new().fg(t.text).bold(),
        )),
        Line::from(""),
        Line::from(vec![
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
                "luvus task add \"…\" --paths src/x/** --gate \"cargo test\"",
                Style::new().fg(t.overlay0),
            )),
        ]);
    }
    draw_centered_empty(f, area, lines);
}

fn draw_automation_empty(f: &mut RenderTarget, area: Rect, cat: &Catalog, t: &Theme) {
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
            cat.automation_empty,
            Style::new().fg(t.text).bold(),
        )),
        Line::from(""),
        Line::from(vec![
            key("a"),
            text(format!(" {}  ·  ", cat.act_new)),
            key("e"),
            text(format!(" {}  ·  ", cat.board_automation_toggle)),
            key("r"),
            text(format!(" {}  ·  ", cat.board_start)),
            key("o"),
            text(format!(" {}", cat.board_details)),
        ]),
    ];
    if area.height >= 5 {
        lines.extend([
            Line::from(""),
            Line::from(Span::styled(
                "luvus automation create --help",
                Style::new().fg(t.overlay0),
            )),
        ]);
    }
    draw_centered_empty(f, area, lines);
}

fn draw_centered_empty(f: &mut RenderTarget, area: Rect, lines: Vec<Line<'_>>) {
    let rows = centered_rows(area, lines.len() as u16);
    f.render_widget(Paragraph::new(lines).alignment(Alignment::Center), rows);
}

fn centered_rows(area: Rect, height: u16) -> Rect {
    let height = height.min(area.height);
    Rect::new(
        area.x,
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        area.width,
        height,
    )
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
                ]),
                Line::from(vec![
                    prefix(16),
                    Span::styled("├─ ", border),
                    Span::styled(format!("↺ {}", cat.task_failed), Style::new().fg(t.coral)),
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
            ]),
            Line::from(vec![
                prefix(13),
                Span::styled("├─ ", border),
                Span::styled(format!("↺ {}", cat.task_failed), Style::new().fg(t.coral)),
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
    if let Some(prompt) = task
        .prompt
        .as_deref()
        .filter(|prompt| !prompt.trim().is_empty())
    {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(" {}", cat.board_f_prompt),
            Style::new().fg(t.subtext1).bold(),
        )));
        lines.extend(prompt.lines().take(3).map(|line| {
            Line::from(Span::styled(
                format!("  {line}"),
                Style::new().fg(t.subtext0),
            ))
        }));
    }
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

/// Keep the insertion cursor visible for a long single-line form value. This
/// crops from the left by terminal display cells and never splits a wide glyph.
fn input_tail(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if super::display_width(value) <= width {
        return value.to_string();
    }
    let budget = width.saturating_sub(1);
    let mut tail = Vec::new();
    let mut used = 0;
    for character in value.chars().rev() {
        let mut encoded = [0; 4];
        let character_width = super::display_width(character.encode_utf8(&mut encoded));
        if used + character_width > budget {
            break;
        }
        tail.push(character);
        used += character_width;
    }
    tail.reverse();
    format!("…{}", tail.into_iter().collect::<String>())
}

/// Split text into explicit display-width rows so scrolling and the visible
/// insertion cursor agree without relying on unstable widget measurements.
fn wrap_display_lines(value: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    for logical in value.split('\n') {
        if logical.is_empty() {
            rows.push(String::new());
            continue;
        }
        let mut remaining = logical;
        while !remaining.is_empty() {
            let mut used = 0;
            let mut end = 0;
            for (index, character) in remaining.char_indices() {
                let mut encoded = [0; 4];
                let character_width = super::display_width(character.encode_utf8(&mut encoded));
                if end > 0 && used + character_width > width.max(1) {
                    break;
                }
                used += character_width;
                end = index + character.len_utf8();
            }
            if end >= remaining.len() {
                rows.push(remaining.to_string());
                break;
            }
            let candidate = &remaining[..end];
            let split = candidate
                .char_indices()
                .rev()
                .find_map(|(index, character)| {
                    (index > 0 && character.is_whitespace()).then_some(index)
                })
                .unwrap_or(end);
            rows.push(candidate[..split].to_string());
            let mut consumed = split;
            if split < end {
                consumed += remaining[split..]
                    .chars()
                    .take_while(|character| character.is_whitespace())
                    .map(char::len_utf8)
                    .sum::<usize>();
            }
            remaining = &remaining[consumed..];
        }
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

/// The in-TUI creation form. Task and automation tabs share the common fields,
/// while keeping task dependencies and automation timing out of each other's
/// workflow. The active field is highlighted with a cursor. Drawn last, over a
/// dimmed backdrop, like the other modals.
pub(super) fn draw_form(
    f: &mut RenderTarget,
    area: Rect,
    form: &OrchForm,
    cat: &Catalog,
    t: &Theme,
) -> Vec<(crate::app::OrchHit, Rect)> {
    let mut hits = Vec::with_capacity(12);
    dim_backdrop(f, area, t);
    let w = area.width.saturating_sub(6).clamp(44, 76).min(area.width);
    let row_count = form
        .fields()
        .iter()
        .map(|field| {
            if *field == crate::app::OrchFormField::Prompt {
                3
            } else {
                1
            }
        })
        .sum::<u16>();
    let modal = centered_rect(area, w, row_count.saturating_add(7).min(area.height));
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
                " {}",
                match form.kind {
                    crate::app::OrchFormKind::Task => cat.board_new_task,
                    crate::app::OrchFormKind::Automation => cat.board_new_automation,
                }
            ),
            Style::new().fg(t.text).bold(),
        )),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    let tab_y = inner.y.saturating_add(2);
    let task_label = format!(" {} ", cat.board_tasks.to_uppercase());
    let automation_label = format!(" {} ", cat.board_automations.to_uppercase());
    let task_w = super::display_width(&task_label) as u16;
    let automation_w = super::display_width(&automation_label) as u16;
    let task_rect = Rect::new(inner.x.saturating_add(1), tab_y, task_w.min(inner.width), 1);
    let automation_rect = Rect::new(
        task_rect.right().saturating_add(1),
        tab_y,
        automation_w.min(inner.right().saturating_sub(task_rect.right() + 1)),
        1,
    );
    let tab = |label: String, selected: bool| {
        Span::styled(
            label,
            if selected {
                Style::new().fg(t.base).bg(t.accent).bold()
            } else {
                Style::new().fg(t.subtext0)
            },
        )
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(" "),
            tab(task_label, form.kind == crate::app::OrchFormKind::Task),
            Span::raw(" "),
            tab(
                automation_label,
                form.kind == crate::app::OrchFormKind::Automation,
            ),
        ])),
        Rect::new(inner.x, tab_y, inner.width, 1),
    );
    hits.push((
        crate::app::OrchHit::FormKind(crate::app::OrchFormKind::Task),
        task_rect,
    ));
    hits.push((
        crate::app::OrchHit::FormKind(crate::app::OrchFormKind::Automation),
        automation_rect,
    ));

    let mut field_y = inner.y.saturating_add(4);
    for field in form.fields().iter().copied() {
        let (label, base_hint) = match field {
            crate::app::OrchFormField::Title => (cat.board_f_title, cat.board_h_title),
            crate::app::OrchFormField::Paths => (cat.board_f_paths, cat.board_h_paths),
            crate::app::OrchFormField::Deps => (cat.board_f_deps, cat.board_h_deps),
            crate::app::OrchFormField::Gate => (cat.board_f_gate, cat.board_h_gate),
            crate::app::OrchFormField::Prompt => (cat.board_f_prompt, cat.board_h_prompt),
            crate::app::OrchFormField::Target => (cat.board_run_with, "left/right"),
            crate::app::OrchFormField::ActiveAgent => {
                (cat.board_f_agent, "left/right: active agents")
            }
            crate::app::OrchFormField::Agent => (cat.board_f_agent, cat.board_h_agent),
            crate::app::OrchFormField::RunIn => (cat.board_run_in, ""),
            crate::app::OrchFormField::Access => (cat.board_access, cat.board_h_access),
            crate::app::OrchFormField::Start => (cat.board_f_start, cat.board_h_start),
            crate::app::OrchFormField::Schedule => (cat.board_f_schedule, cat.board_h_schedule),
        };
        let active = field == form.field;
        let label_style = if active {
            Style::new().fg(t.accent).bold()
        } else {
            Style::new().fg(t.subtext0)
        };
        // A subtle hint of what each field expects, shown when it's empty.
        let start_label = match form.start {
            crate::app::OrchFormStart::Manual => cat.automation_manual,
            crate::app::OrchFormStart::Now => cat.automation_now,
            crate::app::OrchFormStart::Once => cat.automation_once,
            crate::app::OrchFormStart::Hourly => cat.automation_hourly,
            crate::app::OrchFormStart::Daily => cat.automation_daily,
            crate::app::OrchFormStart::Weekly => cat.automation_weekly,
        };
        let value = if field == crate::app::OrchFormField::Target {
            match form.automation_target {
                crate::app::OrchAutomationTarget::NewWorker => {
                    cat.automation_target_new.to_string()
                }
                crate::app::OrchAutomationTarget::ActiveAgent => {
                    cat.automation_target_active.to_string()
                }
            }
        } else if field == crate::app::OrchFormField::Start {
            if form.kind == crate::app::OrchFormKind::Automation && !form.timezone.is_empty() {
                format!("{start_label} · {}", form.timezone)
            } else {
                start_label.to_string()
            }
        } else if field == crate::app::OrchFormField::RunIn {
            match form.mode {
                crate::orch::TaskWorkerMode::Worktree => cat.board_worktree.to_string(),
                crate::orch::TaskWorkerMode::Workspace => cat.board_workspace.to_string(),
            }
        } else if field == crate::app::OrchFormField::Access {
            match form.access {
                crate::automation::AutomationAccess::ReadOnly => {
                    cat.automation_access_read_only.to_string()
                }
                crate::automation::AutomationAccess::Workspace => {
                    cat.automation_access_workspace.to_string()
                }
                crate::automation::AutomationAccess::FullAccess => {
                    cat.automation_access_full.to_string()
                }
            }
        } else {
            form.value(field).to_string()
        };
        let hint = if field == crate::app::OrchFormField::Schedule
            && form.kind == crate::app::OrchFormKind::Automation
        {
            match form.start {
                crate::app::OrchFormStart::Once => "YYYY-MM-DD HH:MM",
                crate::app::OrchFormStart::Hourly => "MM (00-59)",
                crate::app::OrchFormStart::Daily => "HH:MM",
                crate::app::OrchFormStart::Weekly => "mon,fri HH:MM",
                crate::app::OrchFormStart::Manual | crate::app::OrchFormStart::Now => base_hint,
            }
        } else {
            base_hint
        };
        let field_height = if field == crate::app::OrchFormField::Prompt {
            3
        } else {
            1
        };
        let field_rect = Rect::new(inner.x, field_y, inner.width, field_height);
        if field == crate::app::OrchFormField::Prompt {
            const LABEL_WIDTH: u16 = 11;
            f.render_widget(
                Paragraph::new(Span::styled(format!(" {label:<8}: "), label_style)),
                Rect::new(
                    field_rect.x,
                    field_rect.y,
                    LABEL_WIDTH.min(field_rect.width),
                    1,
                ),
            );
            let body_rect = Rect::new(
                field_rect.x.saturating_add(LABEL_WIDTH),
                field_rect.y,
                field_rect.width.saturating_sub(LABEL_WIDTH),
                field_rect.height,
            );
            let (mut display_value, value_style) = if value.is_empty() && !active {
                (hint.to_string(), Style::new().fg(t.overlay0))
            } else {
                (value, Style::new().fg(t.text))
            };
            if active {
                display_value.push('▏');
            }
            let lines = wrap_display_lines(&display_value, body_rect.width as usize);
            let scroll = lines.len().saturating_sub(body_rect.height as usize);
            let visible = lines.into_iter().skip(scroll).map(|line| {
                if let Some(text) = active.then(|| line.strip_suffix('▏')).flatten() {
                    Line::from(vec![
                        Span::styled(text.to_string(), value_style),
                        Span::styled("▏", Style::new().fg(t.accent)),
                    ])
                } else {
                    Line::from(Span::styled(line, value_style))
                }
            });
            f.render_widget(Paragraph::new(visible.collect::<Vec<_>>()), body_rect);
        } else {
            const LABEL_WIDTH: u16 = 11;
            f.render_widget(
                Paragraph::new(Span::styled(format!(" {label:<8}: "), label_style)),
                Rect::new(
                    field_rect.x,
                    field_rect.y,
                    LABEL_WIDTH.min(field_rect.width),
                    1,
                ),
            );
            let body_rect = Rect::new(
                field_rect.x.saturating_add(LABEL_WIDTH),
                field_rect.y,
                field_rect.width.saturating_sub(LABEL_WIDTH),
                1,
            );
            let cursor_width = usize::from(active && body_rect.width > 0);
            let available = body_rect.width as usize - cursor_width;
            let body = if value.is_empty() && !active {
                Span::styled(
                    super::truncate(hint, available),
                    Style::new().fg(t.overlay0),
                )
            } else if active {
                Span::styled(input_tail(&value, available), Style::new().fg(t.text))
            } else {
                Span::styled(super::truncate(&value, available), Style::new().fg(t.text))
            };
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    body,
                    Span::styled(if active { "▏" } else { "" }, Style::new().fg(t.accent)),
                ])),
                body_rect,
            );
        }
        hits.push((crate::app::OrchHit::FormField(field), field_rect));
        field_y = field_y.saturating_add(field_height);
    }

    let bottom = inner.bottom().saturating_sub(1);
    if let Some(e) = &form.error {
        f.render_widget(
            Paragraph::new(Span::styled(format!(" {e}"), Style::new().fg(t.coral))),
            Rect::new(inner.x, bottom, inner.width, 1),
        );
    } else {
        let mut shortcuts = vec![("⏎", cat.act_create)];
        if form.field == crate::app::OrchFormField::Prompt {
            shortcuts.push(("⇧⏎", cat.settings.shift_newline));
        }
        shortcuts.extend([
            ("⇥", cat.board_switch_type),
            ("↑↓", cat.board_move_field),
            ("←→", cat.act_select),
            ("esc", cat.act_cancel),
        ]);
        f.render_widget(
            Paragraph::new(super::hint_line(&shortcuts, t)),
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
        let body_bottom = inner.bottom().saturating_sub(1);
        let body_row = |offset: u16| {
            let y = inner.y.saturating_add(offset);
            (y < body_bottom).then_some(Rect::new(inner.x, y, inner.width, 1))
        };
        if let Some(rect) = body_row(1) {
            f.render_widget(
                Paragraph::new(Span::styled(
                    format!(" {}", cat.board_run_in.to_uppercase()),
                    Style::new().fg(t.overlay1).bold(),
                )),
                rect,
            );
        }
        for (i, (mode, label)) in [
            (crate::orch::TaskWorkerMode::Worktree, cat.board_worktree),
            (crate::orch::TaskWorkerMode::Workspace, cat.board_workspace),
        ]
        .into_iter()
        .enumerate()
        {
            let selected = mode == start.mode;
            let Some(rect) = body_row(2 + i as u16) else {
                continue;
            };
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
            if let Some(rect) = body_row(4) {
                f.render_widget(
                    Paragraph::new(Span::styled(
                        format!("  {warning}"),
                        Style::new().fg(t.amber),
                    )),
                    rect,
                );
            }
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
    let mut lines: Vec<Line> = vec![Line::from(vec![
        Span::styled(format!(" {} ", task.id), Style::new().fg(t.subtext1).bold()),
        Span::styled(status_label(task.status, cat), Style::new().fg(sc)),
    ])];
    lines.extend(
        wrap_display_lines(&task.title, inner.width.saturating_sub(1) as usize)
            .into_iter()
            .map(|title| {
                Line::from(Span::styled(
                    format!(" {title}"),
                    Style::new().fg(t.text).bold(),
                ))
            }),
    );
    lines.push(Line::from(""));
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
    if let Some(prompt) = task
        .prompt
        .as_deref()
        .filter(|prompt| !prompt.trim().is_empty())
    {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(" {}", cat.board_f_prompt),
            Style::new().fg(t.subtext1).bold(),
        )));
        for logical in prompt.split('\n') {
            lines.extend(
                wrap_display_lines(logical, inner.width.saturating_sub(2) as usize)
                    .into_iter()
                    .map(|line| {
                        Line::from(Span::styled(format!("  {line}"), Style::new().fg(t.text)))
                    }),
            );
        }
    }
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
        hits: vec![
            (crate::app::OrchHit::DetailClose, close),
            (crate::app::OrchHit::DetailModal, modal),
        ],
    }
}

pub(super) struct AutomationDetail<'a> {
    pub automation: &'a Automation,
    pub state: &'a AutomationState,
    pub preview: &'a [u64],
}

pub(super) fn draw_automation_detail(
    f: &mut RenderTarget,
    area: Rect,
    detail: AutomationDetail<'_>,
    scroll: usize,
    cat: &Catalog,
    t: &Theme,
) -> DetailRender {
    let AutomationDetail {
        automation,
        state,
        preview,
    } = detail;
    dim_backdrop(f, area, t);
    let w = area.width.saturating_sub(6).clamp(44, 84).min(area.width);
    let h = area.height.saturating_sub(4).clamp(10, 28).min(area.height);
    let modal = centered_rect(area, w, h);
    f.render_widget(Clear, modal);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(t.border_focus).bg(t.surface0))
        .style(Style::new().bg(t.surface0));
    let inner = block.inner(modal);
    f.render_widget(block, modal);

    let (status, color) = automation_status(state, &automation.id, cat, t);
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!(" {} ", automation.id),
                Style::new().fg(t.subtext1).bold(),
            ),
            Span::styled(status, Style::new().fg(color)),
            Span::styled(
                format!("  {}", automation.name),
                Style::new().fg(t.text).bold(),
            ),
        ]),
        Line::from(""),
    ];
    let kv = |key: &str, value: String, lines: &mut Vec<Line>| {
        if !value.is_empty() {
            lines.push(Line::from(vec![
                Span::styled(format!(" {}", pad(key, 12)), Style::new().fg(t.subtext0)),
                Span::styled(value, Style::new().fg(t.text)),
            ]));
        }
    };
    kv(
        cat.board_f_schedule,
        schedule_label(&automation.trigger),
        &mut lines,
    );
    if let Trigger::Daily { timezone, .. } | Trigger::Weekly { timezone, .. } = &automation.trigger
    {
        kv(cat.automation_timezone, timezone.clone(), &mut lines);
    }
    kv(
        cat.col_next_utc,
        automation
            .next_run_at
            .map(super::format_utc)
            .unwrap_or_else(|| "—".into()),
        &mut lines,
    );
    kv(
        cat.board_f_agent,
        automation.task.agent_id.clone(),
        &mut lines,
    );
    kv(
        cat.board_run_with,
        match &automation.target {
            crate::automation::AutomationTarget::NewWorker => cat.automation_target_new.to_string(),
            crate::automation::AutomationTarget::ActiveAgent {
                pane_id, if_busy, ..
            } => format!(
                "{} · p{} · {}",
                cat.automation_target_active,
                pane_id,
                match if_busy {
                    crate::automation::ActiveAgentBusyPolicy::Wait => "wait when busy",
                    crate::automation::ActiveAgentBusyPolicy::Skip => "skip when busy",
                }
            ),
        },
        &mut lines,
    );
    if let crate::automation::AutomationTarget::ActiveAgent { durable, .. } = &automation.target {
        kv(
            cat.automation_binding,
            if durable.is_some() {
                cat.automation_survives_restart
            } else {
                cat.automation_until_restart
            }
            .to_string(),
            &mut lines,
        );
    }
    kv(
        cat.board_workspace,
        automation.task.workspace_id.clone(),
        &mut lines,
    );
    if matches!(
        automation.target,
        crate::automation::AutomationTarget::NewWorker
    ) {
        kv(
            cat.board_run_in,
            match automation.task.mode {
                crate::orch::TaskWorkerMode::Worktree => cat.board_worktree,
                crate::orch::TaskWorkerMode::Workspace => cat.board_workspace,
            }
            .to_string(),
            &mut lines,
        );
        kv(
            cat.board_f_paths,
            automation.task.paths.join(" "),
            &mut lines,
        );
        kv(
            cat.board_f_gate,
            automation.task.gate.clone().unwrap_or_default(),
            &mut lines,
        );
    }
    kv(
        cat.automation_policy,
        format!(
            "misfire={} · overlap={} · grace={}s",
            misfire_label(automation.policy.misfire),
            overlap_label(automation.policy.overlap),
            automation.policy.misfire_grace_seconds
        ),
        &mut lines,
    );
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(" {} · {}", cat.board_f_schedule, cat.automation_next_five),
        Style::new().fg(t.subtext1).bold(),
    )));
    for deadline in preview {
        lines.push(Line::from(Span::styled(
            format!("  {}", super::format_utc(*deadline)),
            Style::new().fg(t.overlay1),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(" {}", cat.board_f_prompt),
        Style::new().fg(t.subtext1).bold(),
    )));
    lines.push(Line::from(Span::styled(
        format!("  {}", automation.task.prompt),
        Style::new().fg(t.text),
    )));

    let runs = state
        .runs
        .iter()
        .rev()
        .filter(|run| run.automation_id == automation.id)
        .take(20)
        .collect::<Vec<_>>();
    if !runs.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(" {}", cat.automation_history),
            Style::new().fg(t.subtext1).bold(),
        )));
        for run in runs {
            let task = run.task_id.as_deref().unwrap_or("—");
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {}", pad(run_status_label(run.status, cat), 10)),
                    Style::new().fg(run_status_color(run.status, t)),
                ),
                Span::styled(
                    format!("{} · {task}", super::format_utc(run.scheduled_at)),
                    Style::new().fg(t.overlay1),
                ),
            ]));
            if let Some(error) = &run.error {
                lines.push(Line::from(Span::styled(
                    format!("    {error}"),
                    Style::new().fg(t.coral),
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
    let visible = body.height as usize;
    let scroll = scroll.min(lines.len().saturating_sub(visible));
    f.render_widget(
        Paragraph::new(lines)
            .scroll((scroll as u16, 0))
            .wrap(Wrap { trim: false }),
        body,
    );
    let footer = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
    let open_label = format!(" ⏎ {} ", cat.automation_open_target);
    let open_rect = Rect::new(
        footer.x,
        footer.y,
        (super::display_width(&open_label) as u16).min(footer.width),
        1,
    );
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(open_label, Style::new().fg(t.accent).bold()),
            Span::styled(
                format!("  j/k {}  ·  esc {}", cat.act_select, cat.act_close),
                Style::new().fg(t.overlay0),
            ),
        ])),
        footer,
    );
    DetailRender {
        scroll,
        hits: vec![
            (crate::app::OrchHit::DetailOpenTarget, open_rect),
            (crate::app::OrchHit::DetailModal, modal),
        ],
    }
}

fn misfire_label(policy: crate::automation::MisfirePolicy) -> &'static str {
    match policy {
        crate::automation::MisfirePolicy::Skip => "skip",
        crate::automation::MisfirePolicy::RunLatest => "run_latest",
    }
}

fn overlap_label(policy: crate::automation::OverlapPolicy) -> &'static str {
    match policy {
        crate::automation::OverlapPolicy::Skip => "skip",
        crate::automation::OverlapPolicy::QueueOne => "queue_one",
    }
}

fn run_status_label(status: RunStatus, cat: &Catalog) -> &str {
    match status {
        RunStatus::Pending => cat.task_queued,
        RunStatus::Starting => cat.automation_starting,
        RunStatus::Running => cat.task_running,
        RunStatus::Review => cat.task_review,
        RunStatus::Delivered => cat.automation_delivered,
        RunStatus::Succeeded => cat.task_done,
        RunStatus::Failed => cat.task_failed,
        RunStatus::Skipped => cat.automation_skipped,
        RunStatus::Cancelled => cat.automation_cancelled,
    }
}

fn run_status_color(status: RunStatus, t: &Theme) -> Color {
    match status {
        RunStatus::Pending | RunStatus::Starting => t.accent,
        RunStatus::Running | RunStatus::Delivered | RunStatus::Succeeded => t.green,
        RunStatus::Review => t.amber,
        RunStatus::Failed => t.coral,
        RunStatus::Skipped | RunStatus::Cancelled => t.overlay1,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automation_table_uses_spaced_headers_and_reports_the_target_pane() {
        let theme = Theme::quattro_rally();
        let header_area = Rect::new(0, 0, 120, 1);
        let mut header_buffer = Buffer::empty(header_area);
        let mut header_target = RenderTarget::new(&mut header_buffer, header_area);
        draw_automation_header(
            &mut header_target,
            header_area,
            automation_columns(header_area.width),
            &crate::i18n::EN,
            &theme,
        );
        let header = (0..header_area.width)
            .map(|x| header_buffer[(x, 0)].symbol())
            .collect::<String>();
        assert_eq!(header.find("ID"), Some(2));
        assert_eq!(header.find("STATUS"), Some(9));
        assert_eq!(header.find("TITLE"), Some(21));
        assert_eq!(header.find("SCHEDULE"), Some(42));
        assert_eq!(header.find("NEXT UTC"), Some(65));
        assert_eq!(header.find("AGENT"), Some(89));
        assert_eq!(header.find("PANE"), Some(102));
        assert!(!header.contains('…'));

        let mut state = AutomationState::default();
        state.automations.push(Automation {
            id: "a1".into(),
            name: "Morning review".into(),
            enabled: true,
            trigger: Trigger::Once { at_utc: 100 },
            target: crate::automation::AutomationTarget::ActiveAgent {
                pane_id: 7,
                terminal_id: "0123456789abcdef0123456789abcdef".into(),
                if_busy: crate::automation::ActiveAgentBusyPolicy::Wait,
                durable: None,
            },
            task: crate::automation::TaskTemplate {
                title: "Review".into(),
                prompt: "Review changes".into(),
                agent_id: "codex".into(),
                workspace_id: "workspace-1".into(),
                mode: crate::orch::TaskWorkerMode::Workspace,
                access: crate::automation::AutomationAccess::Workspace,
                paths: Vec::new(),
                gate: None,
            },
            policy: crate::automation::AutomationPolicy::default(),
            next_run_at: Some(100),
            created_at: 1,
            updated_at: 1,
        });
        let table_area = Rect::new(0, 0, 120, 12);
        let mut table_buffer = Buffer::empty(table_area);
        let mut table_target = RenderTarget::new(&mut table_buffer, table_area);
        render_automations(
            &mut table_target,
            table_area,
            AutomationRender {
                state: &state,
                cursor: 0,
                compact: false,
                catalog: &crate::i18n::EN,
                theme: &theme,
            },
            Vec::new(),
        );
        let cells = |start: u16, width: u16| {
            (start..start + width)
                .map(|x| table_buffer[(x, 3)].symbol())
                .collect::<String>()
        };
        assert_eq!(table_buffer[(1, 3)].symbol(), "▌");
        assert_eq!(cells(90, 5), "codex");
        assert_eq!(cells(103, 2), "p7");
        assert_eq!(table_buffer[(90, 3)].fg, theme.mint);
        assert_eq!(table_buffer[(103, 3)].fg, theme.amber);
        assert_eq!(table_buffer[(90, 3)].bg, theme.sel_bg);
    }

    #[test]
    fn overflowing_automation_table_draws_a_proportional_position_rail() {
        let theme = Theme::quattro_rally();
        let area = Rect::new(0, 0, 80, 10);
        let mut state = AutomationState::default();
        for index in 0..12 {
            state.automations.push(Automation {
                id: format!("a{}", index + 1),
                name: format!("Review {}", index + 1),
                enabled: true,
                trigger: Trigger::Once { at_utc: 100 },
                target: crate::automation::AutomationTarget::NewWorker,
                task: crate::automation::TaskTemplate {
                    title: "Review".into(),
                    prompt: "Review changes".into(),
                    agent_id: "codex".into(),
                    workspace_id: "workspace-1".into(),
                    mode: crate::orch::TaskWorkerMode::Workspace,
                    access: crate::automation::AutomationAccess::Workspace,
                    paths: Vec::new(),
                    gate: None,
                },
                policy: crate::automation::AutomationPolicy::default(),
                next_run_at: Some(100),
                created_at: 1,
                updated_at: 1,
            });
        }
        let mut buffer = Buffer::empty(area);
        let mut target = RenderTarget::new(&mut buffer, area);
        let rendered = render_automations(
            &mut target,
            area,
            AutomationRender {
                state: &state,
                cursor: 0,
                compact: false,
                catalog: &crate::i18n::EN,
                theme: &theme,
            },
            Vec::new(),
        );

        assert_eq!(rendered.scroll, 0);
        // With the fleet summary hidden, area 80x10 gives a six-row viewport
        // and reserves x=78 for the rail.
        assert_eq!(buffer[(78, 3)].bg, theme.overlay1);
        assert_eq!(buffer[(78, 6)].bg, theme.surface1);
        assert!(rendered.hits.iter().all(|(_, rect)| rect.right() <= 78));
    }

    #[test]
    fn fleet_status_is_hidden_but_its_renderer_remains_available() {
        let theme = Theme::quattro_rally();
        let area = Rect::new(0, 0, 120, 20);
        let mut state = AutomationState::default();
        state.automations.push(Automation {
            id: "a1".into(),
            name: "Morning review".into(),
            enabled: true,
            trigger: Trigger::Once { at_utc: 100 },
            target: crate::automation::AutomationTarget::NewWorker,
            task: crate::automation::TaskTemplate {
                title: "Review".into(),
                prompt: "Review changes".into(),
                agent_id: "codex".into(),
                workspace_id: "workspace-1".into(),
                mode: crate::orch::TaskWorkerMode::Workspace,
                access: crate::automation::AutomationAccess::Workspace,
                paths: Vec::new(),
                gate: None,
            },
            policy: crate::automation::AutomationPolicy::default(),
            next_run_at: Some(100),
            created_at: 1,
            updated_at: 1,
        });
        let mut buffer = Buffer::empty(area);
        let mut target = RenderTarget::new(&mut buffer, area);
        render(
            &mut target,
            area,
            &OrchState::default(),
            &state,
            0,
            0,
            0,
            OrchView::Automations,
            crate::orch::TaskWorkerMode::Worktree,
            false,
            None,
            &crate::i18n::EN,
            &theme,
        );
        let automation_view = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!automation_view.contains("1 scheduled"));

        let mut task_buffer = Buffer::empty(area);
        let mut task_target = RenderTarget::new(&mut task_buffer, area);
        render(
            &mut task_target,
            area,
            &OrchState::default(),
            &AutomationState::default(),
            0,
            0,
            0,
            OrchView::Tasks,
            crate::orch::TaskWorkerMode::Worktree,
            false,
            None,
            &crate::i18n::EN,
            &theme,
        );
        let task_view = task_buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!task_view.contains("0 queued"));

        let summary_area = Rect::new(0, 0, 120, 1);
        let mut summary_buffer = Buffer::empty(summary_area);
        let mut summary_target = RenderTarget::new(&mut summary_buffer, summary_area);
        draw_status_summary(
            &mut summary_target,
            summary_area,
            OrchView::Automations,
            &[0; 9],
            &state,
            &crate::i18n::EN,
            &theme,
        );
        let summary = summary_buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(summary.contains("1 scheduled"));
    }

    #[test]
    fn automation_detail_explains_a_durable_active_agent_binding() {
        let area = Rect::new(0, 0, 100, 30);
        let mut buffer = Buffer::empty(area);
        let mut target = RenderTarget::new(&mut buffer, area);
        let mut state = AutomationState::default();
        state.automations.push(Automation {
            id: "a1".into(),
            name: "Continue review".into(),
            enabled: true,
            trigger: Trigger::Once { at_utc: 100 },
            target: crate::automation::AutomationTarget::ActiveAgent {
                pane_id: 7,
                terminal_id: "0123456789abcdef0123456789abcdef".into(),
                if_busy: crate::automation::ActiveAgentBusyPolicy::Wait,
                durable: Some(crate::automation::DurableAgentIdentity {
                    agent_id: "codex".into(),
                    native_session_id: "private-session".into(),
                    workspace_id: "workspace-1".into(),
                    cwd: std::path::PathBuf::from("/workspace"),
                }),
            },
            task: crate::automation::TaskTemplate {
                title: "Continue review".into(),
                prompt: "Continue".into(),
                agent_id: "codex".into(),
                workspace_id: "workspace-1".into(),
                mode: crate::orch::TaskWorkerMode::Workspace,
                access: crate::automation::AutomationAccess::Workspace,
                paths: Vec::new(),
                gate: None,
            },
            policy: crate::automation::AutomationPolicy::default(),
            next_run_at: Some(100),
            created_at: 1,
            updated_at: 1,
        });

        draw_automation_detail(
            &mut target,
            area,
            AutomationDetail {
                automation: &state.automations[0],
                state: &state,
                preview: &[],
            },
            0,
            &crate::i18n::EN,
            &Theme::quattro_rally(),
        );

        let rendered = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Binding"));
        assert!(rendered.contains("Survives server restart"));
        assert!(!rendered.contains("private-session"));
    }

    #[test]
    fn task_form_keeps_the_multiline_prompt_tail_visible() {
        let area = Rect::new(0, 0, 90, 30);
        let mut buffer = Buffer::empty(area);
        let mut target = RenderTarget::new(&mut buffer, area);
        let mut form = OrchForm::for_kind(crate::app::OrchFormKind::Task);
        form.field = crate::app::OrchFormField::Prompt;
        form.prompt = "first line\nsecond line\nthird line\nfourth line".into();

        draw_form(
            &mut target,
            area,
            &form,
            &crate::i18n::EN,
            &Theme::quattro_rally(),
        );

        let rendered = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!rendered.contains("first line"));
        assert!(rendered.contains("second line"));
        assert!(rendered.contains("third line"));
        assert!(rendered.contains("fourth line▏"));
        assert!(rendered.contains("newline"));
    }

    #[test]
    fn long_task_title_keeps_its_tail_and_cursor_visible() {
        let area = Rect::new(0, 0, 52, 22);
        let mut buffer = Buffer::empty(area);
        let mut target = RenderTarget::new(&mut buffer, area);
        let mut form = OrchForm::for_kind(crate::app::OrchFormKind::Task);
        form.field = crate::app::OrchFormField::Title;
        form.title = "A long descriptive task title whose ending stays visible".into();

        draw_form(
            &mut target,
            area,
            &form,
            &crate::i18n::EN,
            &Theme::quattro_rally(),
        );

        let rendered = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("…"));
        assert!(rendered.contains("ending stays visible▏"));
        assert!(!rendered.contains("A long descriptive task title"));
    }

    #[test]
    fn task_detail_shows_the_full_wrapped_title_and_prompt() {
        let area = Rect::new(0, 0, 52, 24);
        let mut buffer = Buffer::empty(area);
        let mut target = RenderTarget::new(&mut buffer, area);
        let mut state = OrchState::default();
        let task = state
            .add_task_with_prompt(
                "Review the complete authentication migration and rollback path".into(),
                Some("Inspect the API contract.\nCover data rollback and recovery.".into()),
                vec![],
                vec![],
                None,
            )
            .unwrap();

        draw_detail(
            &mut target,
            area,
            &task,
            0,
            &crate::i18n::EN,
            &Theme::quattro_rally(),
        );
        let rendered = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Review the complete authentication"));
        assert!(rendered.contains("migration and rollback path"));
        assert!(rendered.contains("Inspect the API contract."));
        assert!(rendered.contains("Cover data rollback and recovery."));
    }

    #[test]
    fn empty_task_and_automation_messages_share_centered_emphasis() {
        let theme = Theme::quattro_rally();

        let task_area = Rect::new(0, 0, 80, 15);
        let mut task_buffer = Buffer::empty(task_area);
        let mut task_target = RenderTarget::new(&mut task_buffer, task_area);
        draw_empty(&mut task_target, task_area, &crate::i18n::EN, &theme);
        let task_y = 5;
        let task_x =
            (task_area.width as usize - super::display_width(crate::i18n::EN.board_empty)) / 2;
        let task_cell = &task_buffer[(task_x as u16, task_y)];
        assert_eq!(task_cell.symbol(), "N");
        assert_eq!(task_cell.fg, theme.text);
        assert!(task_cell.modifier.contains(ratatui::style::Modifier::BOLD));

        let automation_area = Rect::new(0, 0, 80, 20);
        let mut automation_buffer = Buffer::empty(automation_area);
        let mut automation_target = RenderTarget::new(&mut automation_buffer, automation_area);
        render_automations(
            &mut automation_target,
            automation_area,
            AutomationRender {
                state: &AutomationState::default(),
                cursor: 0,
                compact: false,
                catalog: &crate::i18n::EN,
                theme: &theme,
            },
            Vec::new(),
        );
        let automation_text = crate::i18n::EN.automation_empty;
        let automation_row: String = (0..automation_area.width)
            .map(|x| automation_buffer[(x, 8)].symbol())
            .collect();
        assert_eq!(automation_row.trim(), automation_text);
        let automation_x = automation_row.find('N').expect("centered automation text");
        let trailing =
            automation_area.width as usize - automation_x - super::display_width(automation_text);
        assert!(automation_x.abs_diff(trailing) <= 1);
        let automation_cell = &automation_buffer[(automation_x as u16, 8)];
        assert_eq!(automation_cell.symbol(), "N");
        assert_eq!(automation_cell.fg, theme.text);
        assert!(automation_cell
            .modifier
            .contains(ratatui::style::Modifier::BOLD));
        let command_row: String = (0..automation_area.width)
            .map(|x| automation_buffer[(x, 12)].symbol())
            .collect();
        assert_eq!(command_row.trim(), "luvus automation create --help");
    }

    #[test]
    fn compact_flow_keeps_gate_failure_and_success_branches_visible() {
        for mode in [
            crate::orch::TaskWorkerMode::Worktree,
            crate::orch::TaskWorkerMode::Workspace,
        ] {
            let area = Rect::new(0, 0, 31, 18);
            let mut buffer = Buffer::empty(area);
            let mut target = RenderTarget::new(&mut buffer, area);
            draw_flow(
                &mut target,
                area,
                mode,
                &crate::i18n::EN,
                &Theme::quattro_rally(),
            );
            let rows = (0..area.height)
                .map(|y| {
                    (0..area.width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>();
            let gate_row = rows
                .iter()
                .position(|row| row.contains("QUALITY GATE"))
                .expect("quality gate remains visible after narrowing");
            let failure_row = rows
                .iter()
                .position(|row| row.contains("↺ failed"))
                .expect("complete failure branch remains visible after narrowing");
            assert_ne!(gate_row, failure_row);
            match mode {
                crate::orch::TaskWorkerMode::Worktree => {
                    assert!(rows.iter().any(|row| row.contains("MERGE")));
                    assert!(rows.iter().any(|row| row.contains("◆ MERGED")));
                }
                crate::orch::TaskWorkerMode::Workspace => {
                    assert!(rows.iter().any(|row| row.contains("◆ DONE")));
                }
            }
        }
    }

    #[test]
    fn short_mode_picker_registers_hits_only_inside_its_body() {
        let start = crate::app::OrchStart {
            task: "t1".into(),
            cursor: 0,
            step: crate::app::OrchStartStep::Mode,
            mode: crate::orch::TaskWorkerMode::Workspace,
            shared_workers: 0,
        };
        for height in 6..=10 {
            let area = Rect::new(0, 0, 60, height);
            let mut buffer = Buffer::empty(area);
            let mut target = RenderTarget::new(&mut buffer, area);
            let hits = draw_start(
                &mut target,
                area,
                &start,
                &crate::i18n::EN,
                &Theme::quattro_rally(),
            );
            let modal_height = 8.min(area.height.saturating_sub(2).max(4));
            let modal = centered_rect(area, 44.min(area.width), modal_height);
            let inner = Block::new().borders(Borders::ALL).inner(modal);
            let footer_y = inner.bottom().saturating_sub(1);

            for (_, rect) in hits
                .iter()
                .filter(|(hit, _)| matches!(hit, crate::app::OrchHit::StartMode(_)))
            {
                assert!(rect.y >= inner.y, "height {height}: {rect:?}");
                assert!(rect.bottom() <= footer_y, "height {height}: {rect:?}");
            }
            if height == 6 {
                assert!(hits
                    .iter()
                    .all(|(hit, _)| !matches!(hit, crate::app::OrchHit::StartMode(_))));
            }
        }
    }
}
