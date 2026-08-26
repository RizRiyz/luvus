use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::app::App;
use crate::ui::theme::{State, Theme};
use crate::ui::{display_width, truncate, RenderTarget};

use super::MobileLayout;

pub(crate) fn render_header(f: &mut RenderTarget, layout: MobileLayout, app: &mut App, t: &Theme) {
    // This surface replaces the desktop tab bar. Clear symbols as well as
    // styling so a previous wider frame cannot bleed through its blank cells.
    f.render_widget(Clear, layout.header);
    f.render_widget(Block::new().style(Style::new().bg(t.crust)), layout.header);
    app.switcher_button_rect = Some(layout.menu_button);
    app.switcher_close_rect = None;
    app.bar.clear_geometry();

    let hovered = app.hover.is_some_and(|(x, y)| {
        x >= layout.menu_button.x
            && x < layout.menu_button.right()
            && y >= layout.menu_button.y
            && y < layout.menu_button.bottom()
    });
    let (button_fg, button_bg) = if hovered {
        (t.crust, t.accent)
    } else {
        (t.accent, t.surface0)
    };
    f.render_widget(
        Block::new().style(Style::new().bg(button_bg)),
        layout.menu_button,
    );
    f.render_widget(
        Paragraph::new(vec![
            Line::from(app.catalog.act_open_menu.to_uppercase()),
            Line::from(app.catalog.menu.to_uppercase()),
        ])
        .alignment(Alignment::Center)
        .style(Style::new().fg(button_fg).bg(button_bg).bold()),
        layout.menu_button,
    );

    let info_width = layout
        .header
        .width
        .saturating_sub(layout.menu_button.width + 1) as usize;
    if info_width == 0 {
        return;
    }
    let ws = app.ws();
    let tab_index = ws.active_tab;
    let tab_count = ws.tabs.len();
    let tab_name = ws
        .tabs
        .get(tab_index)
        .and_then(|tab| tab.name.as_deref())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{} {}/{}", app.catalog.act_tab, tab_index + 1, tab_count));
    let row_one = format!("{} · {}", ws.name, tab_name);

    let focus = app.layout().focus;
    let pane_count = app.layout().len();
    let pane_position = app
        .layout()
        .leaf_position(focus)
        .map_or(1, |index| index + 1);
    let pane_switch = Rect::new(
        layout.header.x + 1,
        layout.header.y + 1,
        info_width as u16,
        1,
    );
    if pane_count > 1 {
        let previous_width = pane_switch.width / 2;
        app.mobile_pane_prev_rect = Some(Rect::new(
            pane_switch.x,
            pane_switch.y,
            previous_width,
            pane_switch.height,
        ));
        app.mobile_pane_next_rect = Some(Rect::new(
            pane_switch.x + previous_width,
            pane_switch.y,
            pane_switch.width - previous_width,
            pane_switch.height,
        ));
        f.render_widget(Block::new().style(Style::new().bg(t.surface0)), pane_switch);
        for rect in [app.mobile_pane_prev_rect, app.mobile_pane_next_rect]
            .into_iter()
            .flatten()
            .filter(|rect| {
                app.hover.is_some_and(|(x, y)| {
                    x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom()
                })
            })
        {
            f.render_widget(Block::new().style(Style::new().bg(t.surface1)), rect);
        }
    }
    let (state, agent) = app
        .status
        .get(&focus)
        .map(|status| {
            let label = if status.agent.is_empty() {
                app.catalog.pane.to_string()
            } else {
                status.agent.to_uppercase()
            };
            (status.state, label)
        })
        .unwrap_or((State::Unknown, app.catalog.pane.to_string()));
    let notification = app.mobile_bar_notification();
    let has_notification = notification.is_some();
    let agent_summary = app.mobile_agent_summary();
    let summary_state = agent_summary.as_ref().map(|(_, state)| *state);
    let summary = if let Some(notification) = notification {
        notification
    } else if let Some((summary, _)) = agent_summary {
        summary
    } else if app.update_available.is_some() {
        app.catalog.update_available.to_string()
    } else {
        String::new()
    };
    let left_text = format!("{} · {}/{}", agent, pane_position, pane_count.max(1));
    let summary = truncate(&summary, info_width / 2);
    let summary_width = display_width(&summary);
    let fixed_width = if pane_count > 1 { 6 } else { 2 };
    let left_budget = if summary.is_empty() {
        info_width.saturating_sub(fixed_width)
    } else {
        info_width.saturating_sub(summary_width + fixed_width + 1)
    };

    f.render_widget(
        Paragraph::new(Span::styled(
            truncate(&row_one, info_width),
            Style::new().fg(t.text).bold(),
        )),
        Rect::new(layout.header.x + 1, layout.header.y, info_width as u16, 1),
    );
    if layout.header.height < 2 {
        return;
    }
    let mut spans = Vec::new();
    if pane_count > 1 {
        spans.push(Span::styled("‹ ", Style::new().fg(t.accent).bold()));
    }
    spans.extend([
        Span::styled(state.dot(), Style::new().fg(state.color(t))),
        Span::styled(
            format!(" {}", truncate(&left_text, left_budget)),
            Style::new().fg(t.subtext1),
        ),
    ]);
    if pane_count > 1 {
        spans.push(Span::styled(" ›", Style::new().fg(t.accent).bold()));
    }
    if !summary.is_empty() {
        let used = spans
            .iter()
            .map(|span| display_width(span.content.as_ref()))
            .sum::<usize>();
        let gap = info_width.saturating_sub(used + summary_width);
        spans.push(Span::raw(" ".repeat(gap)));
        let color = if !has_notification {
            summary_state.map_or(t.accent, |state| state.color(t))
        } else {
            t.accent
        };
        spans.push(Span::styled(summary, Style::new().fg(color)));
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(
            layout.header.x + 1,
            layout.header.y + 1,
            info_width as u16,
            1,
        ),
    );
}
