//! The Settings modal: a centered, tabbed dialog over a dimmed backdrop, in the
//! macOS System-Preferences toolbar style. Drawn last (on top of everything)
//! when open; returns the hit-test rects `render()` stores on the `App`.

use super::*;
use crate::app::{GeneralRow, LayoutRow, ModuleRow, SettingsTab};
use crate::module::manifest::SettingKind;
use ratatui::widgets::{Borders, Clear};

pub(super) struct SettingsHits {
    pub modal: Rect,
    pub close: Rect,
    pub tabs: Vec<(SettingsTab, Rect)>,
    pub ctls: Vec<(usize, Rect)>,
    pub theme_remove: Vec<(String, Rect)>,
    pub arrows: Vec<(usize, i32, Rect)>,
    pub layout_scroll: usize,
}

fn format_history_budget(bytes: usize) -> String {
    let mib = crate::config::MIB;
    if bytes.is_multiple_of(mib) {
        format!("{} MiB", bytes / mib)
    } else {
        format!("{:.1} MiB", bytes as f64 / mib as f64)
    }
}

fn diff_layout_label(value: crate::diff::DiffLayoutPreference, app: &App) -> &'static str {
    match value {
        crate::diff::DiffLayoutPreference::Auto => app.catalog.settings.diff_auto,
        crate::diff::DiffLayoutPreference::Split => app.catalog.settings.diff_split,
        crate::diff::DiffLayoutPreference::Stack => app.catalog.settings.diff_stack,
    }
}

fn diff_marker_label(value: crate::diff::DiffMarkerStyle, app: &App) -> &'static str {
    match value {
        crate::diff::DiffMarkerStyle::Symbols => app.catalog.settings.diff_symbols,
        crate::diff::DiffMarkerStyle::Bars => app.catalog.settings.diff_bars,
        crate::diff::DiffMarkerStyle::Both => app.catalog.settings.diff_both,
    }
}

fn diff_color_label(value: crate::diff::DiffColorMode, app: &App) -> &'static str {
    match value {
        crate::diff::DiffColorMode::Theme => app.catalog.settings.diff_theme,
        crate::diff::DiffColorMode::Standard => app.catalog.settings.diff_red_green,
    }
}

fn key_reference_label(section: usize, row: usize, app: &App) -> String {
    let cat = app.catalog.settings;
    match (section, row) {
        (0, 5) => cat.key_prefix_twice.replace("{prefix}", cat.keys_prefix),
        (1, 2) | (2, 2) => format!("{} / b", cat.key_space),
        (2, 0) | (3, 0) => format!("{}  hjkl", cat.key_arrows),
        (3, 1) => cat.key_shift_arrow.to_string(),
        (7, 0) => cat.key_drag.to_string(),
        (7, 1) => cat.key_shift_drag.to_string(),
        (8, 0) => cat.key_click.to_string(),
        (8, 1) => cat.key_right_click.to_string(),
        (8, 2) => cat.key_wheel.to_string(),
        (8, 3) => cat.key_drag_divider.to_string(),
        (8, 4) => cat.key_tap_pane.to_string(),
        _ => crate::i18n::settings::KEY_REFERENCE_KEYS[section][row].to_string(),
    }
}

pub(super) fn draw_settings(
    f: &mut RenderTarget,
    area: Rect,
    app: &App,
    t: &Theme,
) -> SettingsHits {
    dim_backdrop(f, area, t);

    // Width must fit the whole tab bar — translated labels (esp. CJK) can be much
    // wider than English, so size to the tabs instead of a fixed cap. The tabs
    // are ` {icon} {label} ` pills; the loop starts at inner.x+1 (1 left margin),
    // and the modal adds 2 border columns. Keep a little breathing room beyond
    // the toolbar so wide controls (notably Luvus Bar placement) do not crowd
    // the border. Both dimensions remain capped to the current viewport.
    let tabs_w: u16 = SettingsTab::ALL
        .iter()
        .map(|st| display_width(&format!(" {} {} ", st.icon(), st.label(app.catalog))) as u16)
        .sum();
    let w = (tabs_w + 12).max(54).min(area.width);
    let h = area.height.saturating_sub(2).clamp(16, 30).min(area.height);
    let modal = if app.compact {
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

    let (tab, cursor, layout_scroll) = app
        .settings
        .as_ref()
        .map(|u| (u.tab, u.cursor, u.layout_scroll))
        .unwrap_or((SettingsTab::Theme, 0, 0));

    // ── title bar ──
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(" "),
            Span::styled(app.catalog.settings_title, Style::new().fg(t.text).bold()),
        ])),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let close = Rect::new(inner.right().saturating_sub(3), inner.y, 3, 1);
    f.render_widget(
        Paragraph::new(Span::styled(" ✕ ", Style::new().fg(t.accent).bold())),
        close,
    );
    if inner.height > 1 {
        hline(f, inner.x, inner.y + 1, inner.width, t);
    }

    let mut tabs = Vec::new();
    // A complete settings layout needs room for the title, tabs, separators,
    // at least one content row, and the footer. Very short phone terminals do
    // not have that space, so show only the active section instead of creating
    // tab and rule rectangles beyond the buffer. Keyboard section switching
    // remains available and a later resize restores the full layout.
    if inner.height < 9 {
        if inner.height > 2 && inner.width > 0 {
            let rect = Rect::new(inner.x, inner.y + 2, inner.width, 1);
            let label = format!("{} {}", tab.icon(), tab.label(app.catalog));
            f.render_widget(
                Paragraph::new(Span::styled(
                    truncate(&label, rect.width as usize),
                    Style::new().fg(t.crust).bg(t.accent).bold(),
                ))
                .alignment(Alignment::Center),
                rect,
            );
            tabs.push((tab, rect));
        }
        return SettingsHits {
            modal,
            close,
            tabs,
            ctls: Vec::new(),
            theme_remove: Vec::new(),
            arrows: Vec::new(),
            layout_scroll,
        };
    }

    // ── tab toolbar (Mac-style pills) ──
    let ty = inner.y + 2;
    let tab_rows = if app.compact {
        SettingsTab::ALL.len().div_ceil(3) as u16
    } else {
        1
    };
    if app.compact {
        let cell_width = (inner.width / 3).max(1);
        let grid_width = cell_width.saturating_mul(3);
        let grid_x = inner.x + inner.width.saturating_sub(grid_width) / 2;
        for (index, st) in SettingsTab::ALL.into_iter().enumerate() {
            let row = index / 3;
            let column = index % 3;
            let rect = Rect::new(
                grid_x + column as u16 * cell_width,
                ty + row as u16,
                cell_width,
                1,
            );
            let style = if st == tab {
                Style::new().fg(t.crust).bg(t.accent).bold()
            } else {
                Style::new().fg(t.subtext0)
            };
            let label = format!("{} {}", st.icon(), st.label(app.catalog));
            f.render_widget(
                Paragraph::new(Span::styled(truncate(&label, cell_width as usize), style))
                    .alignment(Alignment::Center),
                rect,
            );
            tabs.push((st, rect));
        }
    } else {
        let mut x = inner.x + 1;
        for st in SettingsTab::ALL {
            let label = format!(" {} {} ", st.icon(), st.label(app.catalog));
            let cw = display_width(&label) as u16;
            if x + cw > inner.right() {
                break;
            }
            let style = if st == tab {
                Style::new().fg(t.crust).bg(t.accent).bold()
            } else {
                Style::new().fg(t.subtext0)
            };
            let rect = Rect::new(x, ty, cw, 1);
            f.render_widget(Paragraph::new(Span::styled(label, style)), rect);
            tabs.push((st, rect));
            x += cw;
        }
    }
    let tabs_bottom = inner.y + 2 + tab_rows;
    hline(f, inner.x, tabs_bottom, inner.width, t);

    // ── content ──
    let content_y = tabs_bottom + 1;
    let content_bottom = inner.bottom().saturating_sub(2);
    let content = Rect::new(
        inner.x,
        content_y,
        inner.width,
        content_bottom.saturating_sub(content_y),
    );
    let (ctls, theme_remove, arrows, layout_scroll) =
        draw_content(f, content, tab, cursor, layout_scroll, app, t);

    // ── footer hint (Keys tab gets its own rebind/reset hints) ──
    let footer_y = inner.bottom().saturating_sub(1);
    hline(f, inner.x, footer_y.saturating_sub(1), inner.width, t);
    let c = app.catalog;
    // On the Keys tab the hints depend on which row the cursor is on: the prefix
    // row and the rebindable commands capture (⏎) and reset (⌫); the preset row
    // cycles (←→); a reference row (past the command list) just scrolls.
    let hdr = crate::app::KEYS_HEADER_ROWS;
    let on_prefix = tab == SettingsTab::Keys && cursor == crate::app::KEYS_PREFIX_ROW;
    let on_preset = tab == SettingsTab::Keys && cursor == crate::app::KEYS_PRESET_ROW;
    let on_command =
        tab == SettingsTab::Keys && cursor >= hdr && cursor < hdr + crate::app::Cmd::ALL.len();
    let hints: &[(&str, &str)] = if on_prefix || on_command {
        &[
            ("↑↓", c.act_move),
            ("⇥", c.act_section),
            ("⏎", c.act_rebind),
            ("⌫", c.act_reset),
            ("esc", c.act_close),
        ]
    } else if on_preset {
        &[
            ("↑↓", c.act_move),
            ("←→", c.act_adjust),
            ("⏎", c.act_apply),
            ("esc", c.act_close),
        ]
    } else if tab == SettingsTab::Keys {
        &[
            ("↑↓", c.act_move),
            ("⇥", c.act_section),
            ("esc", c.act_close),
        ]
    } else {
        &[
            ("↑↓", c.act_move),
            ("⇥", c.act_tab),
            ("←→", c.act_adjust),
            ("⏎", c.act_apply),
            ("esc", c.act_close),
        ]
    };
    f.render_widget(
        Paragraph::new(hint_line(hints, t)),
        Rect::new(inner.x, footer_y, inner.width, 1),
    );

    SettingsHits {
        modal,
        close,
        tabs,
        ctls,
        theme_remove,
        arrows,
        layout_scroll,
    }
}

type Content = (
    Vec<(usize, Rect)>,
    Vec<(String, Rect)>,
    Vec<(usize, i32, Rect)>,
    usize,
);

fn draw_content(
    f: &mut RenderTarget,
    area: Rect,
    tab: SettingsTab,
    cursor: usize,
    layout_scroll: usize,
    app: &App,
    t: &Theme,
) -> Content {
    let mut ctls = Vec::new();
    let mut theme_remove = Vec::new();
    let mut arrows = Vec::new();
    let mut resolved_layout_scroll = layout_scroll;
    let cat = app.catalog;
    match tab {
        SettingsTab::Theme => {
            // Scroll the list so the selected theme is always visible (there are
            // more palettes than fit a short modal).
            let avail = area.height.max(1) as usize;
            let themes = app.theme_registry.entries();
            let total = themes.len();
            let scroll = cursor
                .saturating_sub(avail.saturating_sub(1))
                .min(total.saturating_sub(avail));
            // Size the name column to the longest registered name, so the swatches
            // and descriptions stay in one straight column however long a palette
            // is called. A fixed width mis-aligned every row once a name outgrew it.
            let name_w = themes
                .iter()
                .map(|entry| display_width(&entry.id))
                .max()
                .unwrap_or(9);
            for (vi, i) in (scroll..total).take(avail).enumerate() {
                let entry = &themes[i];
                let name = entry.id.as_str();
                let row = Rect::new(area.x, area.y + vi as u16, area.width, 1);
                let sel = i == cursor;
                if sel {
                    fill_bg(f, row, t.sel_bg);
                }
                // Two swatches: the palette's background, then its accent.
                // Background matters because related flavours (the Catppuccin set)
                // share an accent and differ only in how dark the surface is — an
                // accent-only swatch made them indistinguishable. `by_name` returns
                // full RGB; downsample when the active theme is (i.e. on
                // non-truecolor terminals) so it renders the right color instead of
                // a mangled truecolor escape.
                let pal = &entry.theme;
                let (mut bg, mut accent) = (pal.base, pal.accent);
                if app.downsample {
                    bg = crate::ipc::protocol::to_256(bg);
                    accent = crate::ipc::protocol::to_256(accent);
                }
                // Local files get the same right-aligned installed/remove affordance
                // as agent integrations. Bundled and virtual themes never expose a
                // destructive action. Reserve its cells so descriptions cannot draw
                // underneath the button.
                let remove = (matches!(
                    entry.source,
                    crate::theme::registry::ThemeSource::Local { .. }
                ) && !app.theme_uninstall_pending(&entry.id))
                .then(|| {
                    let installed = format!("✓ {} ", cat.act_installed);
                    let action = format!("· ⏎ {}", cat.settings.remove);
                    let width = (display_width(&installed) + display_width(&action)) as u16;
                    let width = width.min(row.width.saturating_sub(1));
                    let rect = Rect::new(
                        row.right().saturating_sub(width.saturating_add(1)),
                        row.y,
                        width,
                        1,
                    );
                    f.render_widget(
                        Paragraph::new(Line::from(vec![
                            Span::styled(installed, Style::new().fg(t.mint)),
                            Span::styled(action, Style::new().fg(t.overlay0)),
                        ]))
                        .alignment(Alignment::Right),
                        rect,
                    );
                    theme_remove.push((entry.id.clone(), rect));
                    rect
                });
                let content_width = remove
                    .map(|rect| rect.x.saturating_sub(row.x + 1))
                    .unwrap_or(row.width);
                f.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled(if sel { " ▸ " } else { "   " }, Style::new().fg(t.accent)),
                        Span::styled(
                            format!("{name:<w$}", w = name_w),
                            Style::new().fg(if sel { t.text } else { t.subtext1 }),
                        ),
                        Span::raw(" "),
                        Span::styled("   ", Style::new().bg(bg)),
                        Span::styled("   ", Style::new().bg(accent)),
                        Span::raw("  "),
                        Span::styled(
                            if entry.description.is_empty() {
                                entry.display_name.as_str()
                            } else {
                                entry.description.as_str()
                            },
                            Style::new().fg(t.overlay0),
                        ),
                    ])),
                    Rect::new(row.x, row.y, content_width, 1),
                );
                ctls.push((i, row));
            }
        }
        SettingsTab::Language => {
            // Mirror the Theme list: each row shows the language's *own* name so a
            // user who can't read English still recognizes it.
            let avail = area.height.max(1) as usize;
            let total = crate::i18n::LANGS.len();
            let scroll = cursor
                .saturating_sub(avail.saturating_sub(1))
                .min(total.saturating_sub(avail));
            for (vi, i) in (scroll..total).take(avail).enumerate() {
                let code = crate::i18n::LANGS[i];
                let name = crate::i18n::native_name(code);
                let row = Rect::new(area.x, area.y + vi as u16, area.width, 1);
                let sel = i == cursor;
                if sel {
                    fill_bg(f, row, t.sel_bg);
                }
                // Pad by display width so CJK names (width-2 cells) still align.
                let pad = " ".repeat(18usize.saturating_sub(display_width(name)));
                f.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled(if sel { " ▸ " } else { "   " }, Style::new().fg(t.accent)),
                        Span::styled(
                            format!("{name}{pad}"),
                            Style::new().fg(if sel { t.text } else { t.subtext1 }),
                        ),
                        Span::styled(code.to_string(), Style::new().fg(t.overlay0)),
                    ])),
                    row,
                );
                ctls.push((i, row));
            }
        }
        SettingsTab::Layout => {
            // Pane-layout rows, then a blank gap + `── Docks ──` divider, then the
            // sidebar + dock-placement rows. The list scrolls to keep the cursor
            // visible (docs/29), so a long registry of plugin docks stays reachable.
            let rows = app.layout_rows();
            let dock_start = app.dock_section_start();
            let diff_start = app.diff_section_start();
            let bar_start = app.bar_section_start();
            let l = &app.config.layout;
            // Visual sequence: control rows plus a blank + divider before the docks.
            enum V {
                Ctl(usize),
                Blank,
                Divider(&'static str),
            }
            let mut vis = Vec::new();
            for i in 0..rows.len() {
                if i == diff_start {
                    vis.push(V::Blank);
                    vis.push(V::Divider(cat.tab_diff));
                }
                if i == dock_start {
                    vis.push(V::Blank);
                    vis.push(V::Divider(cat.tab_docks));
                }
                if i == bar_start {
                    vis.push(V::Blank);
                    vis.push(V::Divider(cat.tab_luvus_bar));
                }
                vis.push(V::Ctl(i));
            }
            let avail = area.height.max(1) as usize;
            let cur_vis = vis
                .iter()
                .position(|v| matches!(v, V::Ctl(i) if *i == cursor))
                .unwrap_or(0);
            let scroll = keep_visible_scroll(layout_scroll, cur_vis, avail, vis.len());
            resolved_layout_scroll = scroll;
            for (row_i, v) in vis.iter().enumerate().skip(scroll).take(avail) {
                let y = area.y + (row_i - scroll) as u16;
                let i = match v {
                    V::Blank => continue,
                    V::Divider(label) => {
                        hline(f, area.x, y, area.width, t);
                        f.render_widget(
                            Paragraph::new(Span::styled(
                                format!(" {label} "),
                                Style::new().fg(t.subtext0).bg(t.surface0),
                            )),
                            Rect::new(
                                area.x + 2,
                                y,
                                (display_width(label) as u16 + 2).min(area.width),
                                1,
                            ),
                        );
                        continue;
                    }
                    V::Ctl(i) => *i,
                };
                match &rows[i] {
                    LayoutRow::SidebarWidth => {
                        let r = slider_row(
                            f,
                            area,
                            y,
                            i,
                            cursor == i,
                            cat.set_left_sidebar_width,
                            app.sidebars.left.width.to_string(),
                            t,
                            &mut arrows,
                        );
                        ctls.push((i, r));
                    }
                    LayoutRow::ColGap => {
                        ctls.push(ctl_row(
                            f,
                            area,
                            y,
                            i,
                            cursor,
                            cat.set_column_gap,
                            toggle(l.col_gap == 1, t),
                            t,
                        ));
                    }
                    LayoutRow::RowGap => {
                        ctls.push(ctl_row(
                            f,
                            area,
                            y,
                            i,
                            cursor,
                            cat.set_row_gap,
                            toggle(l.row_gap == 1, t),
                            t,
                        ));
                    }
                    LayoutRow::Scrollback => {
                        let r = slider_row(
                            f,
                            area,
                            y,
                            i,
                            cursor == i,
                            cat.set_scrollback,
                            format_history_budget(app.config.scrollback_bytes()),
                            t,
                            &mut arrows,
                        );
                        ctls.push((i, r));
                    }
                    LayoutRow::MobileWidth => {
                        let r = slider_row(
                            f,
                            area,
                            y,
                            i,
                            cursor == i,
                            cat.set_mobile_width,
                            if l.mobile_width == 0 {
                                cat.side_off.to_string()
                            } else {
                                l.mobile_width.to_string()
                            },
                            t,
                            &mut arrows,
                        );
                        ctls.push((i, r));
                    }
                    LayoutRow::PaneTitles => {
                        ctls.push(ctl_row(
                            f,
                            area,
                            y,
                            i,
                            cursor,
                            cat.set_pane_titles,
                            toggle(l.show_titles, t),
                            t,
                        ));
                    }
                    LayoutRow::PaneTitlePath => {
                        ctls.push(ctl_row(
                            f,
                            area,
                            y,
                            i,
                            cursor,
                            cat.set_pane_title_path,
                            toggle(l.pane_title_path, t),
                            t,
                        ));
                    }
                    LayoutRow::ResumeWs => {
                        ctls.push(ctl_row(
                            f,
                            area,
                            y,
                            i,
                            cursor,
                            cat.set_resume_workspace,
                            toggle(l.resume_in_new_workspace, t),
                            t,
                        ));
                    }
                    LayoutRow::DiffLayout => {
                        ctls.push(ctl_row(
                            f,
                            area,
                            y,
                            i,
                            cursor,
                            cat.set_diff_layout,
                            picker(diff_layout_label(app.config.layout.diff_layout, app), t),
                            t,
                        ));
                    }
                    LayoutRow::DiffWrap => {
                        ctls.push(ctl_row(
                            f,
                            area,
                            y,
                            i,
                            cursor,
                            cat.set_diff_wrap,
                            toggle(app.config.layout.diff_wrap, t),
                            t,
                        ));
                    }
                    LayoutRow::DiffContext => {
                        let row = slider_row(
                            f,
                            area,
                            y,
                            i,
                            cursor == i,
                            cat.set_diff_context,
                            app.config.layout.diff_context_lines.to_string(),
                            t,
                            &mut arrows,
                        );
                        ctls.push((i, row));
                    }
                    LayoutRow::DiffLineNumbers => {
                        ctls.push(ctl_row(
                            f,
                            area,
                            y,
                            i,
                            cursor,
                            cat.set_diff_line_numbers,
                            toggle(app.config.layout.diff_show_line_numbers, t),
                            t,
                        ));
                    }
                    LayoutRow::DiffMarkers => {
                        ctls.push(ctl_row(
                            f,
                            area,
                            y,
                            i,
                            cursor,
                            cat.set_diff_markers,
                            picker(
                                diff_marker_label(app.config.layout.diff_marker_style, app),
                                t,
                            ),
                            t,
                        ));
                    }
                    LayoutRow::DiffColors => {
                        ctls.push(ctl_row(
                            f,
                            area,
                            y,
                            i,
                            cursor,
                            cat.set_diff_colors,
                            picker(diff_color_label(app.config.layout.diff_color_mode, app), t),
                            t,
                        ));
                    }
                    LayoutRow::DiffLiveRefresh => {
                        ctls.push(ctl_row(
                            f,
                            area,
                            y,
                            i,
                            cursor,
                            cat.set_diff_live_refresh,
                            toggle(app.config.layout.diff_live_refresh, t),
                            t,
                        ));
                    }
                    #[cfg(windows)]
                    LayoutRow::Shell => {
                        let shell = match app.config.shell.as_str() {
                            "default" => cat.settings.shell_default,
                            "cmd" => cat.settings.shell_command_prompt,
                            choice => crate::platform::shell_label(choice),
                        };
                        ctls.push(ctl_row(
                            f,
                            area,
                            y,
                            i,
                            cursor,
                            cat.settings.shell,
                            picker(shell, t),
                            t,
                        ));
                    }
                    LayoutRow::LeftVisible => {
                        ctls.push(ctl_row(
                            f,
                            area,
                            y,
                            i,
                            cursor,
                            &format!("◧ {}", cat.side_left),
                            toggle(app.sidebars.left.visible, t),
                            t,
                        ));
                    }
                    LayoutRow::RightVisible => {
                        ctls.push(ctl_row(
                            f,
                            area,
                            y,
                            i,
                            cursor,
                            &format!("◨ {}", cat.side_right),
                            toggle(app.sidebars.right.visible, t),
                            t,
                        ));
                    }
                    LayoutRow::RightWidth => {
                        let r = slider_row(
                            f,
                            area,
                            y,
                            i,
                            cursor == i,
                            cat.set_right_sidebar_width,
                            app.sidebars.right.width.to_string(),
                            t,
                            &mut arrows,
                        );
                        ctls.push((i, r));
                    }
                    LayoutRow::Dock(kind) => {
                        ctls.push(dock_row(f, area, y, i, cursor, app, kind, t, &mut arrows));
                    }
                    LayoutRow::Bar(key) => {
                        ctls.push(bar_row(f, area, y, i, cursor, app, key, t, &mut arrows));
                    }
                }
            }
        }
        SettingsTab::General => {
            // The file-open chooser, then a blank gap + `── Notifications ──`
            // divider and the sound rows, mirroring the Layout tab's Docks section.
            let rows = app.general_rows();
            let sec = app.general_section_start();
            let n = &app.config.notifications;
            let file_open = app.file_open_label();
            let mut y = area.y;
            for (i, row) in rows.iter().enumerate() {
                if i == sec {
                    y += 1; // blank line before the section
                    if y >= area.bottom() {
                        break;
                    }
                    hline(f, area.x, y, area.width, t);
                    let label = format!(" {} ", cat.tab_notify);
                    let w = (display_width(&label) as u16).min(area.width);
                    f.render_widget(
                        Paragraph::new(Span::styled(
                            label,
                            Style::new().fg(t.subtext0).bg(t.surface0),
                        )),
                        Rect::new(area.x + 2, y, w, 1),
                    );
                    y += 1;
                }
                if y >= area.bottom() {
                    break;
                }
                match row {
                    GeneralRow::FileOpen => {
                        let r = slider_row(
                            f,
                            area,
                            y,
                            i,
                            cursor == i,
                            cat.set_file_open,
                            file_open.clone(),
                            t,
                            &mut arrows,
                        );
                        ctls.push((i, r));
                    }
                    GeneralRow::FilesShowHidden => ctls.push(ctl_row(
                        f,
                        area,
                        y,
                        i,
                        cursor,
                        cat.set_files_hidden,
                        toggle(app.config.layout.files_show_hidden, t),
                        t,
                    )),
                    GeneralRow::ShiftEnter => {
                        let r = slider_row(
                            f,
                            area,
                            y,
                            i,
                            cursor == i,
                            cat.set_shift_enter,
                            app.shift_enter_label(),
                            t,
                            &mut arrows,
                        );
                        ctls.push((i, r));
                    }
                    GeneralRow::CheckUpdates => ctls.push(ctl_row(
                        f,
                        area,
                        y,
                        i,
                        cursor,
                        cat.set_check_updates,
                        toggle(app.config.check_updates, t),
                        t,
                    )),
                    GeneralRow::ResumeFlags => ctls.push(ctl_row(
                        f,
                        area,
                        y,
                        i,
                        cursor,
                        cat.set_resume_flags,
                        toggle(app.config.resume_launch_flags, t),
                        t,
                    )),
                    GeneralRow::NewPaneToWorkspaceRoot => ctls.push(ctl_row(
                        f,
                        area,
                        y,
                        i,
                        cursor,
                        cat.set_new_pane_to_workspace_root,
                        toggle(app.config.layout.new_pane_to_workspace_root, t),
                        t,
                    )),
                    GeneralRow::AgentTitle => ctls.push(ctl_row(
                        f,
                        area,
                        y,
                        i,
                        cursor,
                        cat.set_agent_title,
                        toggle(app.config.layout.agent_title, t),
                        t,
                    )),
                    GeneralRow::SoundStyle => {
                        let r = slider_row(
                            f,
                            area,
                            y,
                            i,
                            cursor == i,
                            cat.set_sound_style,
                            app.sound_style_label().to_string(),
                            t,
                            &mut arrows,
                        );
                        ctls.push((i, r));
                    }
                    GeneralRow::SoundDone => ctls.push(ctl_row(
                        f,
                        area,
                        y,
                        i,
                        cursor,
                        cat.set_sound_done,
                        toggle(n.sound_on_done, t),
                        t,
                    )),
                    GeneralRow::SoundBlocked => ctls.push(ctl_row(
                        f,
                        area,
                        y,
                        i,
                        cursor,
                        cat.set_sound_blocked,
                        toggle(n.sound_on_blocked, t),
                        t,
                    )),
                    GeneralRow::TestDoneSound => ctls.push(ctl_row(
                        f,
                        area,
                        y,
                        i,
                        cursor,
                        cat.set_test_done_sound,
                        Line::from(Span::styled(
                            format!("[ ♪ {} ]", cat.act_play),
                            Style::new().fg(t.accent).bold(),
                        )),
                        t,
                    )),
                    GeneralRow::TestBlockedSound => ctls.push(ctl_row(
                        f,
                        area,
                        y,
                        i,
                        cursor,
                        cat.set_test_blocked_sound,
                        Line::from(Span::styled(
                            format!("[ ♪ {} ]", cat.act_play),
                            Style::new().fg(t.accent).bold(),
                        )),
                        t,
                    )),
                }
                y += 1;
            }
        }
        SettingsTab::Integrations => {
            for (i, agent) in crate::integration::AGENTS.iter().enumerate() {
                let val = if crate::integration::is_installed(agent) {
                    // Installed → clicking removes luvus's hook (not the agent).
                    Line::from(vec![
                        Span::styled(format!("✓ {} ", cat.act_installed), Style::new().fg(t.mint)),
                        Span::styled(
                            format!("· ⏎ {}", cat.settings.remove),
                            Style::new().fg(t.overlay0),
                        ),
                    ])
                } else {
                    Line::from(Span::styled(
                        format!("[ {} ]", cat.settings.install),
                        Style::new().fg(t.accent).bold(),
                    ))
                };
                ctls.push(ctl_row(
                    f,
                    area,
                    area.y + i as u16,
                    i,
                    cursor,
                    agent,
                    val,
                    t,
                ));
            }
        }
        SettingsTab::Keys => {
            // One scrollable list: a how-to intro, the rebindable prefix commands
            // grouped by section, then read-only reference blocks (fixed keys plus
            // the git tab / task board / picker / mouse shortcuts). The cursor steps
            // through commands *and* reference rows — a shared `selectable index` —
            // so pressing Down eventually reaches every block; notes and headings
            // scroll along but aren't landed on.
            let capturing = app.settings.as_ref().is_some_and(|u| u.capturing);
            let prefix_candidate = app
                .settings
                .as_ref()
                .and_then(|ui| ui.prefix_candidate.as_deref())
                .and_then(crate::app::PrefixSpec::parse)
                .map(|prefix| prefix.label());
            let all = crate::app::Cmd::ALL;
            let prefix_label = app.prefix.label();
            // The preset row's value: the matched localized label, or Custom.
            let preset_label = app
                .current_preset()
                .map(|i| crate::app::presets()[i].localized_label(cat).to_string())
                .unwrap_or_else(|| cat.settings.preset_custom.to_string());

            enum KV {
                Note(String),
                Heading(&'static str),
                Blank,
                // Selectable rows carry their cursor index (`sel`).
                // A labelled value row (the prefix chord / the preset chooser): a
                // fixed label on the left, a live value on the right.
                Value {
                    sel: usize,
                    label: &'static str,
                    value: String,
                    /// Show `‹ value ›` (a chooser) rather than a plain value.
                    chooser: bool,
                },
                Command {
                    sel: usize,
                    cmd: crate::app::Cmd,
                },
                Reference {
                    sel: usize,
                    k: String,
                    d: &'static str,
                },
            }
            // How to use the prefix (the intro block).
            let mut vis: Vec<KV> = vec![
                KV::Note(
                    cat.settings
                        .keys_intro_prefix
                        .replace("{prefix}", &prefix_label),
                ),
                KV::Note(
                    cat.settings
                        .keys_intro_move
                        .replace("{prefix}", &prefix_label),
                ),
                KV::Note(cat.settings.keys_intro_edit.to_string()),
                KV::Blank,
            ];
            // The two command-mode rows: the prefix chord and the preset chooser
            // (docs/64), selectable at rows 0 and 1 (before the commands).
            vis.push(KV::Heading(cat.settings.keys_command_mode));
            vis.push(KV::Value {
                sel: crate::app::KEYS_PREFIX_ROW,
                label: cat.settings.keys_prefix,
                value: prefix_label.clone(),
                chooser: false,
            });
            vis.push(KV::Value {
                sel: crate::app::KEYS_PRESET_ROW,
                label: cat.settings.keys_preset,
                value: preset_label,
                chooser: true,
            });
            vis.push(KV::Blank);
            // The rebindable commands, grouped by section — selectable rows start
            // after the two header rows.
            let mut section = "";
            for (i, cmd) in all.iter().enumerate() {
                let s = cmd.section(cat);
                if s != section {
                    if !section.is_empty() {
                        vis.push(KV::Blank);
                    }
                    vis.push(KV::Heading(s));
                    section = s;
                }
                vis.push(KV::Command {
                    sel: crate::app::KEYS_HEADER_ROWS + i,
                    cmd: *cmd,
                });
            }
            // The read-only reference blocks — selectable indices continue past the
            // commands, so the cursor flows straight from the last command into them.
            let mut sel = crate::app::KEYS_HEADER_ROWS + all.len();
            for (section, keys) in crate::i18n::settings::KEY_REFERENCE_KEYS.iter().enumerate() {
                vis.push(KV::Blank);
                vis.push(KV::Heading(cat.settings.key_reference_headings[section]));
                for (row, (_key, d)) in keys
                    .iter()
                    .zip(cat.settings.key_reference_descriptions[section].iter())
                    .enumerate()
                {
                    vis.push(KV::Reference {
                        sel,
                        k: key_reference_label(section, row, app),
                        d,
                    });
                    sel += 1;
                }
            }

            let avail = area.height.max(1) as usize;
            // Find the row the cursor is on (a command or a reference row) and scroll
            // to keep it visible.
            let cur_vis = vis
                .iter()
                .position(|v| {
                    matches!(v, KV::Command { sel, .. } | KV::Reference { sel, .. } | KV::Value { sel, .. } if *sel == cursor)
                })
                .unwrap_or(0);
            let scroll = cur_vis
                .saturating_sub(avail.saturating_sub(1))
                .min(vis.len().saturating_sub(avail));
            for (row_i, v) in vis.iter().enumerate().skip(scroll).take(avail) {
                let row = Rect::new(area.x, area.y + (row_i - scroll) as u16, area.width, 1);
                match v {
                    KV::Blank => {}
                    KV::Value {
                        sel,
                        label,
                        value,
                        chooser,
                    } => {
                        let is_sel = *sel == cursor;
                        if is_sel {
                            fill_bg(f, row, t.sel_bg);
                        }
                        f.render_widget(
                            Paragraph::new(Line::from(vec![
                                Span::styled(
                                    if is_sel { " ▸ " } else { "   " },
                                    Style::new().fg(t.accent),
                                ),
                                Span::styled(
                                    *label,
                                    Style::new().fg(if is_sel { t.text } else { t.subtext1 }),
                                ),
                            ])),
                            row,
                        );
                        // The prefix row shows a capture prompt while capturing;
                        // the preset row shows `‹ value ›`.
                        let txt = if is_sel && capturing {
                            prefix_candidate
                                .as_ref()
                                .map(|candidate| {
                                    cat.settings.keys_capture_again.replace("{key}", candidate)
                                })
                                .unwrap_or_else(|| cat.settings.keys_capture_prefix.to_string())
                        } else if *chooser {
                            format!("‹ {value} ›")
                        } else {
                            value.clone()
                        };
                        let color = if is_sel && capturing {
                            t.coral
                        } else {
                            t.accent
                        };
                        f.render_widget(
                            Paragraph::new(Span::styled(
                                format!("{txt}  "),
                                Style::new().fg(color).bold(),
                            ))
                            .alignment(Alignment::Right),
                            row,
                        );
                        ctls.push((*sel, row));
                    }
                    KV::Note(text) => {
                        f.render_widget(
                            Paragraph::new(Line::from(vec![
                                Span::raw("   "),
                                Span::styled(text.clone(), Style::new().fg(t.overlay0)),
                            ])),
                            row,
                        );
                    }
                    KV::Heading(h) => {
                        f.render_widget(
                            Paragraph::new(Span::styled(
                                format!("  {h}"),
                                Style::new().fg(t.subtext0).bold(),
                            )),
                            row,
                        );
                    }
                    KV::Reference { sel, k, d } => {
                        if *sel == cursor {
                            fill_bg(f, row, t.sel_bg);
                        }
                        let key_pad = " ".repeat(13usize.saturating_sub(display_width(k)));
                        f.render_widget(
                            Paragraph::new(Line::from(vec![
                                Span::styled(
                                    format!("   {k}{key_pad} "),
                                    Style::new().fg(t.accent).bold(),
                                ),
                                Span::styled(*d, Style::new().fg(t.overlay0)),
                            ])),
                            row,
                        );
                        ctls.push((*sel, row));
                    }
                    KV::Command { sel, cmd } => {
                        let cmd = *cmd;
                        let is_sel = *sel == cursor;
                        if is_sel {
                            fill_bg(f, row, t.sel_bg);
                        }
                        // The command label on the left…
                        f.render_widget(
                            Paragraph::new(Line::from(vec![
                                Span::styled(
                                    if is_sel { " ▸ " } else { "   " },
                                    Style::new().fg(t.accent),
                                ),
                                Span::styled(
                                    cmd.label(cat),
                                    Style::new().fg(if is_sel { t.text } else { t.subtext1 }),
                                ),
                            ])),
                            row,
                        );
                        // …its bound key on the right, or a prompt while capturing.
                        let key = app.key_for(cmd);
                        let (txt, color) = if is_sel && capturing {
                            (cat.settings.keys_capture_key.to_string(), t.coral)
                        } else if key.is_empty() {
                            ("—".to_string(), t.overlay0) // unbound
                        } else {
                            (key, t.accent)
                        };
                        f.render_widget(
                            Paragraph::new(Span::styled(
                                format!("{txt}  "),
                                Style::new().fg(color).bold(),
                            ))
                            .alignment(Alignment::Right),
                            row,
                        );
                        ctls.push((*sel, row));
                    }
                }
            }
        }
        SettingsTab::Modules => {
            let rows = app.module_rows();
            if rows.is_empty() {
                f.render_widget(
                    Paragraph::new(Span::styled(
                        format!("   {}", cat.settings.modules_empty),
                        Style::new().fg(t.overlay0),
                    )),
                    Rect::new(area.x, area.y, area.width, 1),
                );
            } else {
                // Scroll so the cursor stays visible once modules expand their
                // settings past the modal height (same idea as the Layout tab).
                let h = area.height.max(1) as usize;
                let first = cursor.saturating_sub(h.saturating_sub(1));
                for (i, r) in rows.iter().enumerate().skip(first) {
                    let y = area.y + (i - first) as u16;
                    if y >= area.bottom() {
                        break;
                    }
                    let row = Rect::new(area.x, y, area.width, 1);
                    let sel = i == cursor;
                    if sel {
                        fill_bg(f, row, t.sel_bg);
                    }
                    match *r {
                        ModuleRow::Module(mi) => {
                            let Some(m) = app.modules.modules.get(mi) else {
                                continue;
                            };
                            // name + a hint (surface count, or a ⚠ for a load warning)
                            let hint = if m.warning.is_some() {
                                format!(" ⚠ {}", cat.settings.module_unavailable)
                            } else {
                                module_hint(m, cat.settings)
                            };
                            f.render_widget(
                                Paragraph::new(Line::from(vec![
                                    Span::styled(
                                        format!("  {}", m.id),
                                        Style::new().fg(if sel { t.text } else { t.subtext1 }),
                                    ),
                                    Span::styled(hint, Style::new().fg(t.overlay0)),
                                ])),
                                row,
                            );
                            f.render_widget(
                                Paragraph::new(toggle(m.enabled, t)).alignment(Alignment::Right),
                                Rect::new(row.x, row.y, row.width.saturating_sub(2), 1),
                            );
                        }
                        ModuleRow::Setting(mi, si) => {
                            let Some((m, spec)) = app
                                .modules
                                .modules
                                .get(mi)
                                .and_then(|m| m.manifest.settings.get(si).map(|s| (m, s)))
                            else {
                                continue;
                            };
                            let value = crate::module::settings::get(&m.manifest, &m.id, &spec.key)
                                .unwrap_or_else(|| spec.default_value());
                            let shown = crate::module::settings::display(spec, &value);
                            // Indented under its module, with a ╰ tie so a long
                            // settings list still reads as belonging to it.
                            let label = format!("  ╰ {}", spec.title);
                            match spec.kind {
                                // Number/enum step through `‹ ›`, like the Layout sliders.
                                SettingKind::Number | SettingKind::Enum => {
                                    slider_row(
                                        f,
                                        area,
                                        row.y,
                                        i,
                                        sel,
                                        &label,
                                        shown,
                                        t,
                                        &mut arrows,
                                    );
                                }
                                SettingKind::Bool | SettingKind::String => {
                                    f.render_widget(
                                        Paragraph::new(Span::styled(
                                            format!("  {label}"),
                                            Style::new().fg(if sel { t.text } else { t.subtext0 }),
                                        )),
                                        row,
                                    );
                                    if spec.kind == SettingKind::Bool {
                                        let on = value.as_bool().unwrap_or(false);
                                        f.render_widget(
                                            Paragraph::new(toggle(on, t))
                                                .alignment(Alignment::Right),
                                            Rect::new(row.x, row.y, row.width.saturating_sub(2), 1),
                                        );
                                    } else {
                                        f.render_widget(
                                            Paragraph::new(Span::styled(
                                                format!("{shown}  "),
                                                Style::new().fg(t.accent),
                                            ))
                                            .alignment(Alignment::Right),
                                            row,
                                        );
                                    }
                                }
                            }
                        }
                    }
                    ctls.push((i, row));
                }
            }
        }
    }
    (ctls, theme_remove, arrows, resolved_layout_scroll)
}

/// Preserve the current viewport whenever the selected row is already visible;
/// move it only far enough to reveal keyboard navigation beyond either edge.
fn keep_visible_scroll(scroll: usize, cursor: usize, viewport: usize, total: usize) -> usize {
    let viewport = viewport.max(1);
    let mut scroll = scroll.min(total.saturating_sub(viewport));
    if cursor < scroll {
        scroll = cursor;
    } else if cursor >= scroll.saturating_add(viewport) {
        scroll = cursor.saturating_add(1).saturating_sub(viewport);
    }
    scroll.min(total.saturating_sub(viewport))
}

/// The one-line summary of what a module contributes, e.g. `· 2 actions · 1 dock`.
/// Only non-zero surfaces are listed, so a small module stays quiet.
fn module_hint(m: &crate::module::InstalledModule, cat: &crate::i18n::settings::Catalog) -> String {
    let man = &m.manifest;
    let parts = [
        (man.actions.len(), cat.module_action, cat.module_actions),
        (man.panes.len(), cat.module_pane, cat.module_panes),
        (man.docks.len(), cat.module_dock, cat.module_docks),
        (man.settings.len(), cat.module_setting, cat.module_settings),
    ];
    let mut out = String::new();
    for (n, singular, plural) in parts {
        if n > 0 {
            let name = if n == 1 { singular } else { plural };
            out.push_str(&format!(" · {n} {name}"));
        }
    }
    out
}

/// The inline prompt for a `type = "string"` module setting, drawn over the
/// Settings modal. Mirrors the tab/workspace rename modals (docs/28).
pub(super) fn draw_module_setting_prompt(f: &mut RenderTarget, area: Rect, app: &App, t: &Theme) {
    let Some(e) = app.module_setting_edit.as_ref() else {
        return;
    };
    let w = 52.min(area.width.max(10));
    let h = 5.min(area.height.max(3));
    let rect = Rect::new(
        area.x + (area.width.saturating_sub(w)) / 2,
        area.y + (area.height.saturating_sub(h)) / 2,
        w,
        h,
    );
    f.render_widget(Clear, rect);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(t.border_focus).bg(t.surface0))
        .style(Style::new().bg(t.surface0));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    f.render_widget(
        Paragraph::new(Span::styled(
            format!(" {}", e.title),
            Style::new().fg(t.subtext1),
        )),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    // A secret is echoed as bullets, so a shoulder-surfer can't read a token.
    let shown = if e.secret {
        "•".repeat(e.buffer.chars().count())
    } else {
        e.buffer.clone()
    };
    f.render_widget(
        Paragraph::new(Span::styled(
            format!(" {shown}▏"),
            Style::new().fg(t.text).bold(),
        )),
        Rect::new(inner.x, inner.y + 1, inner.width, 1),
    );
    f.render_widget(
        Paragraph::new(Span::styled(
            format!(" {}", app.catalog.settings.module_edit_hint),
            Style::new().fg(t.overlay0),
        )),
        Rect::new(inner.x, inner.y + 2, inner.width, 1),
    );
}

/// The `‹ value ›` slider row for control `idx`. Records the two arrow cells as
/// decrement/increment targets so the left arrow decreases and the right
/// increases.
#[allow(clippy::too_many_arguments)]
fn slider_row(
    f: &mut RenderTarget,
    area: Rect,
    y: u16,
    idx: usize,
    sel: bool,
    label: &str,
    value: String,
    t: &Theme,
    arrows: &mut Vec<(usize, i32, Rect)>,
) -> Rect {
    let row = Rect::new(area.x, y, area.width, 1);
    if sel {
        fill_bg(f, row, t.sel_bg);
    }
    f.render_widget(
        Paragraph::new(Span::styled(
            format!("  {label}"),
            Style::new().fg(if sel { t.text } else { t.subtext1 }),
        )),
        row,
    );
    // Place "‹ value ›" two cells in from the right edge so positions are exact.
    let w = display_width(&format!("‹ {value} ›")) as u16;
    let sx = row.right().saturating_sub(2 + w);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("‹", Style::new().fg(t.accent).bold()),
            Span::styled(format!(" {value} "), Style::new().fg(t.text).bold()),
            Span::styled("›", Style::new().fg(t.accent).bold()),
        ])),
        Rect::new(sx, row.y, w, 1),
    );
    arrows.push((idx, -1, Rect::new(sx, row.y, 2, 1)));
    arrows.push((idx, 1, Rect::new(sx + w.saturating_sub(2), row.y, 2, 1)));
    row
}

/// A label + right-aligned value control row, highlighted when selected.
#[allow(clippy::too_many_arguments)]
fn ctl_row(
    f: &mut RenderTarget,
    area: Rect,
    y: u16,
    i: usize,
    cursor: usize,
    label: &str,
    value: Line<'static>,
    t: &Theme,
) -> (usize, Rect) {
    let row = Rect::new(area.x, y, area.width, 1);
    let sel = i == cursor;
    if sel {
        fill_bg(f, row, t.sel_bg);
    }
    f.render_widget(
        Paragraph::new(Span::styled(
            format!("  {label}"),
            Style::new().fg(if sel { t.text } else { t.subtext1 }),
        )),
        row,
    );
    f.render_widget(
        Paragraph::new(value).alignment(Alignment::Right),
        Rect::new(row.x, row.y, row.width.saturating_sub(2), 1),
    );
    (i, row)
}

/// A dock placement row (docs/29): the dock name on the left, and wide
/// `[Left] [Right]` buttons on the right with the current side highlighted. The
/// buttons are registered as `idx` arrows (`-1` = left, `+1` = right), so a click
/// on either moves the dock — big, obvious targets, not tiny `‹ ›` glyphs.
#[allow(clippy::too_many_arguments)]
fn dock_row(
    f: &mut RenderTarget,
    area: Rect,
    y: u16,
    idx: usize,
    cursor: usize,
    app: &App,
    kind: &crate::app::DockKind,
    t: &Theme,
    arrows: &mut Vec<(usize, i32, Rect)>,
) -> (usize, Rect) {
    let row = Rect::new(area.x, y, area.width, 1);
    let sel = idx == cursor;
    if sel {
        fill_bg(f, row, t.sel_bg);
    }
    f.render_widget(
        Paragraph::new(Span::styled(
            format!("  {}", app.dock_label(kind)),
            Style::new().fg(if sel { t.text } else { t.subtext1 }),
        )),
        row,
    );
    // Three place buttons: [Left] [Right] [Off]. The current state is highlighted;
    // each is registered as an `idx` arrow (-1 = left, +1 = right, +2 = off) so a
    // click routes through the normal settings-adjust path.
    let side = app.sidebars.side_of(kind);
    let cat = app.catalog;
    let btns = [
        (
            format!(" {} ", cat.side_left),
            -1i32,
            side == Some(crate::app::Side::Left),
        ),
        (
            format!(" {} ", cat.side_right),
            1,
            side == Some(crate::app::Side::Right),
        ),
        (format!(" {} ", cat.side_off), 2, side.is_none()),
    ];
    let on = Style::new().fg(t.crust).bg(t.accent).bold();
    let off = Style::new().fg(t.subtext0).bg(t.surface1);
    let total: u16 = btns
        .iter()
        .map(|(l, _, _)| display_width(l) as u16 + 1)
        .sum::<u16>()
        .saturating_sub(1);
    let mut bx = row.right().saturating_sub(2 + total);
    for (label, delta, active) in btns {
        let w = display_width(&label) as u16;
        let r = Rect::new(bx, y, w, 1);
        f.render_widget(
            Paragraph::new(Span::styled(label, if active { on } else { off })),
            r,
        );
        arrows.push((idx, delta, r));
        bx += w + 1;
    }
    (idx, row)
}

#[allow(clippy::too_many_arguments)]
fn bar_row(
    f: &mut RenderTarget,
    area: Rect,
    y: u16,
    idx: usize,
    cursor: usize,
    app: &App,
    key: &str,
    t: &Theme,
    arrows: &mut Vec<(usize, i32, Rect)>,
) -> (usize, Rect) {
    let row = Rect::new(area.x, y, area.width, 1);
    let selected = idx == cursor;
    if selected {
        fill_bg(f, row, t.sel_bg);
    }
    let declaration = app.bar.declaration(key);
    let title = declaration.map_or(key, |declaration| declaration.title.as_str());
    let fallback = declaration.map_or(crate::bar::BarRegion::BottomRight, |declaration| {
        declaration.region
    });
    let region = app.config.bars.region_for(key, fallback);
    let cat = app.catalog;
    f.render_widget(
        Paragraph::new(Span::styled(
            format!("  {title}"),
            Style::new().fg(if selected { t.text } else { t.subtext1 }),
        )),
        row,
    );
    let buttons = [
        (
            format!(" {} ", cat.side_top),
            -1,
            region == Some(crate::bar::BarRegion::TopRight),
        ),
        (
            format!(" {} ", cat.side_bottom),
            1,
            region == Some(crate::bar::BarRegion::BottomRight),
        ),
        (format!(" {} ", cat.side_off), 2, region.is_none()),
    ];
    let total = buttons
        .iter()
        .map(|(label, _, _)| display_width(label) as u16 + 1)
        .sum::<u16>()
        .saturating_sub(1);
    let mut x = row.right().saturating_sub(total + 2);
    for (label, delta, active) in buttons {
        let width = display_width(&label) as u16;
        let rect = Rect::new(x, y, width, 1);
        let style = if active {
            Style::new().fg(t.crust).bg(t.accent).bold()
        } else {
            Style::new().fg(t.subtext0).bg(t.surface1)
        };
        f.render_widget(Paragraph::new(Span::styled(label, style)), rect);
        arrows.push((idx, delta, rect));
        x = x.saturating_add(width + 1);
    }
    (idx, row)
}

/// A `‹ value ›` picker display (cycled by click / keys; no arrow hit-rects).
fn picker(value: &str, t: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled("‹ ", Style::new().fg(t.overlay1)),
        Span::styled(value.to_string(), Style::new().fg(t.accent).bold()),
        Span::styled(" ›", Style::new().fg(t.overlay1)),
    ])
}

fn toggle(on: bool, t: &Theme) -> Line<'static> {
    if on {
        Line::from(Span::styled("[✓]", Style::new().fg(t.accent).bold()))
    } else {
        Line::from(Span::styled("[ ]", Style::new().fg(t.overlay1)))
    }
}

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

fn fill_bg(f: &mut RenderTarget, rect: Rect, color: ratatui::style::Color) {
    let buf = f.buffer_mut();
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            buf[(x, y)].set_bg(color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{display_width, keep_visible_scroll, slider_row, Rect, RenderTarget, Theme};
    use ratatui::buffer::Buffer;

    #[test]
    fn layout_scroll_moves_only_when_selection_leaves_the_viewport() {
        assert_eq!(keep_visible_scroll(10, 15, 12, 40), 10);
        assert_eq!(keep_visible_scroll(10, 10, 12, 40), 10);
        assert_eq!(keep_visible_scroll(10, 9, 12, 40), 9);
        assert_eq!(keep_visible_scroll(10, 22, 12, 40), 11);
        assert_eq!(keep_visible_scroll(30, 15, 12, 20), 8);
    }

    #[test]
    fn slider_uses_terminal_width_for_cjk_value_and_arrow_targets() {
        let area = Rect::new(0, 0, 40, 1);
        let mut buffer = Buffer::empty(area);
        let mut arrows = Vec::new();
        let value = "只读";
        let rendered = format!("‹ {value} ›");
        let width = display_width(&rendered) as u16;
        assert!(width > rendered.chars().count() as u16);

        {
            let mut target = RenderTarget::new(&mut buffer, area);
            slider_row(
                &mut target,
                area,
                0,
                3,
                false,
                "打开文件方式",
                value.to_string(),
                &Theme::noir(),
                &mut arrows,
            );
        }

        let start = area.right().saturating_sub(2 + width);
        assert_eq!(arrows[0], (3, -1, Rect::new(start, 0, 2, 1)));
        assert_eq!(
            arrows[1],
            (3, 1, Rect::new(start + width.saturating_sub(2), 0, 2, 1))
        );
        assert_eq!(buffer[(start, 0)].symbol(), "‹");
        assert_eq!(buffer[(start + width.saturating_sub(1), 0)].symbol(), "›");
    }
}
