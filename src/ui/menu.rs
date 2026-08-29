//! Right-click context menus (workspace + pane), drawn as a small popup anchored
//! at the click point.

use super::*;
use crate::app::{
    AgentMenuItem, DiffMenuItem, FileMenuItem, ModuleMenuAction, PaneMenuItem, TabMenuItem,
    WsMenuItem,
};
use crate::i18n::Catalog;
use ratatui::widgets::{Borders, Clear};

/// One row of a context-menu popup.
struct MenuRow {
    text: String,
    divider: bool,
    destructive: bool,
}

fn row_is_hovered(row: Rect, hover: Option<(u16, u16)>) -> bool {
    hover.is_some_and(|(column, pointer_row)| {
        column >= row.x
            && column < row.right()
            && pointer_row >= row.y
            && pointer_row < row.bottom()
    })
}

/// Render a context-menu popup anchored near `anchor` (clamped so it stays on
/// screen) and return one clickable rect per row — dividers included — in order,
/// for the input layer to hit-test.
fn render_popup(
    f: &mut RenderTarget,
    area: Rect,
    anchor: (u16, u16),
    rows: &[MenuRow],
    hover: Option<(u16, u16)>,
    mobile: bool,
    t: &Theme,
) -> Vec<Rect> {
    let (ax, ay) = anchor;
    // Size the box to the widest label (+ a leading pad + the border).
    let label_w = rows
        .iter()
        .map(|r| super::display_width(&r.text))
        .max()
        .unwrap_or(6) as u16;
    let row_height = if mobile { 2 } else { 1 };
    let w = if mobile {
        area.width.max(1)
    } else {
        (label_w + 3).clamp(12, area.width.max(1))
    };
    let h = ((rows.len() as u16).saturating_mul(row_height) + 2).min(area.height.max(1));
    let x = if mobile {
        area.x
    } else {
        ax.min(area.right().saturating_sub(w)).max(area.x)
    };
    let y = if mobile {
        area.bottom().saturating_sub(h)
    } else {
        ay.min(area.bottom().saturating_sub(h)).max(area.y)
    };
    let popup = Rect::new(x, y, w, h);

    f.render_widget(Clear, popup);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(t.border_focus).bg(t.surface0))
        .style(Style::new().bg(t.surface0));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let mut rects = Vec::with_capacity(rows.len());
    for (i, r) in rows.iter().enumerate() {
        let row = Rect::new(
            inner.x,
            inner.y + i as u16 * row_height,
            inner.width,
            row_height.min(
                inner
                    .bottom()
                    .saturating_sub(inner.y + i as u16 * row_height),
            ),
        );
        if row.height == 0 {
            break;
        }
        let text_row = Rect::new(
            row.x,
            row.y + row.height.saturating_sub(1) / 2,
            row.width,
            1,
        );
        if r.divider {
            // A thin, non-interactive separator across the inner width.
            let line = "─".repeat(inner.width as usize);
            f.render_widget(
                Paragraph::new(Span::styled(
                    line,
                    Style::new().fg(t.surface1).bg(t.surface0),
                )),
                text_row,
            );
            rects.push(row);
            continue;
        }
        let hot = row_is_hovered(row, hover);
        let fg = if hot {
            t.crust
        } else if r.destructive {
            t.coral // the one destructive action
        } else {
            t.text
        };
        let bg = if hot { t.accent } else { t.surface0 };
        f.render_widget(
            Paragraph::new(Span::styled(
                format!(" {}", r.text),
                Style::new().fg(fg).bg(bg),
            )),
            text_row,
        );
        rects.push(row);
    }
    rects
}

pub(super) fn draw_ws_menu(
    f: &mut RenderTarget,
    area: Rect,
    app: &mut App,
    cat: &Catalog,
    t: &Theme,
) {
    let Some(index) = app.ws_menu_target_index() else {
        app.ws_menu = None;
        return;
    };
    let Some(menu) = app.ws_menu.as_ref() else {
        return;
    };
    let anchor = menu.anchor;
    let items = app.ws_menu_items(index);
    let extras = menu.module_actions.clone();
    let rows: Vec<MenuRow> = items
        .iter()
        .map(|it| MenuRow {
            text: ws_label(*it, cat, &extras),
            divider: matches!(it, WsMenuItem::Divider),
            destructive: matches!(it, WsMenuItem::Close | WsMenuItem::DeleteWorktree),
        })
        .collect();
    let rects = render_popup(f, area, anchor, &rows, app.hover, app.compact, t);
    if let Some(menu) = app.ws_menu.as_mut() {
        menu.items = items.into_iter().zip(rects).collect();
    }
}

pub(super) fn draw_tab_menu(
    f: &mut RenderTarget,
    area: Rect,
    app: &mut App,
    cat: &Catalog,
    t: &Theme,
) {
    let Some(menu) = app.tab_menu.as_ref() else {
        return;
    };
    let anchor = menu.anchor;
    let extras = menu.module_actions.clone();
    let swap_targets = menu.swap_targets.clone();
    let previous_swap_rects = menu.swap_rects.clone();

    let items = app.tab_menu_items();
    let rows: Vec<MenuRow> = items
        .iter()
        .map(|item| MenuRow {
            text: tab_label(*item, cat, &extras),
            divider: matches!(item, TabMenuItem::Divider),
            destructive: false,
        })
        .collect();
    let rects = render_popup(f, area, anchor, &rows, app.hover, app.compact, t);
    let swap_rect = items
        .iter()
        .zip(&rects)
        .find(|(item, _)| **item == TabMenuItem::SwapWith)
        .map(|(_, rect)| *rect);
    if let Some(menu) = app.tab_menu.as_mut() {
        menu.items = items.iter().copied().zip(rects.iter().copied()).collect();
    }

    // Keep the submenu open across the one-column gap between both popups.
    if let (Some(parent), Some(hover)) = (swap_rect, app.hover) {
        let in_rect = |rect: &Rect| {
            hover.0 >= rect.x
                && hover.0 < rect.right()
                && hover.1 >= rect.y
                && hover.1 < rect.bottom()
        };
        let over_parent = in_rect(&parent);
        let over_submenu = previous_swap_rects.iter().any(|(_, rect)| in_rect(rect));
        let over_other = items.iter().zip(&rects).any(|(item, rect)| {
            !matches!(item, TabMenuItem::SwapWith | TabMenuItem::Divider) && in_rect(rect)
        });
        if let Some(menu) = app.tab_menu.as_mut() {
            if over_parent || over_submenu {
                menu.swap_open = true;
            } else if over_other {
                menu.swap_open = false;
            }
        }
    }

    let open = app.tab_menu.as_ref().is_some_and(|menu| menu.swap_open);
    if let (Some(parent), false) = (open.then_some(()).and(swap_rect), swap_targets.is_empty()) {
        let sub_rows: Vec<MenuRow> = swap_targets
            .iter()
            .map(|(_, label)| MenuRow {
                text: label.clone(),
                divider: false,
                destructive: false,
            })
            .collect();
        let sub_anchor = (parent.right() + 1, parent.y.saturating_sub(1));
        let sub_rects = render_popup(f, area, sub_anchor, &sub_rows, app.hover, app.compact, t);
        if let Some(menu) = app.tab_menu.as_mut() {
            menu.swap_rects = swap_targets
                .iter()
                .map(|(target, _)| target.clone())
                .zip(sub_rects)
                .collect();
        }
    } else if let Some(menu) = app.tab_menu.as_mut() {
        menu.swap_rects.clear();
    }
}

pub(super) fn draw_pane_menu(
    f: &mut RenderTarget,
    area: Rect,
    app: &mut App,
    cat: &Catalog,
    t: &Theme,
) {
    let Some(menu) = app.pane_menu.as_ref() else {
        return;
    };
    let anchor = menu.anchor;
    let extras = menu.module_actions.clone();
    let move_targets = menu.move_targets.clone();
    // Submenu rects from the *previous* frame, to keep the submenu open while the
    // cursor is over it (before we recompute this frame's rects).
    let prev_tab_rects = menu.tab_rects.clone();

    let items = app.pane_menu_items();
    let rows: Vec<MenuRow> = items
        .iter()
        .map(|it| MenuRow {
            text: pane_label(*it, cat, &extras),
            divider: matches!(it, PaneMenuItem::Divider),
            destructive: matches!(it, PaneMenuItem::Close),
        })
        .collect();
    let rects = render_popup(f, area, anchor, &rows, app.hover, app.compact, t);
    let move_rect = items
        .iter()
        .zip(&rects)
        .find(|(it, _)| **it == PaneMenuItem::MoveToTab)
        .map(|(_, r)| *r);
    if let Some(menu) = app.pane_menu.as_mut() {
        menu.items = items.iter().copied().zip(rects.iter().copied()).collect();
    }

    // Sticky open/close of the submenu based on where the cursor is: over the
    // "Move to tab" row or the submenu opens it; over another main row closes it;
    // over the border gap between them leaves it unchanged (so it doesn't flicker).
    if let (Some(mrect), Some(hov)) = (move_rect, app.hover) {
        let in_r =
            |r: &Rect| hov.0 >= r.x && hov.0 < r.right() && hov.1 >= r.y && hov.1 < r.bottom();
        let over_move = in_r(&mrect);
        let over_submenu = prev_tab_rects.iter().any(|(_, r)| in_r(r));
        let over_other = items.iter().zip(&rects).any(|(it, r)| {
            !matches!(it, PaneMenuItem::MoveToTab | PaneMenuItem::Divider) && in_r(r)
        });
        if let Some(menu) = app.pane_menu.as_mut() {
            if over_move || over_submenu {
                menu.move_open = true;
            } else if over_other {
                menu.move_open = false;
            }
        }
    }

    let open = app.pane_menu.as_ref().is_some_and(|m| m.move_open);
    match (open.then_some(()).and(move_rect), move_targets.is_empty()) {
        (Some(mrect), false) => {
            let sub_rows: Vec<MenuRow> = move_targets
                .iter()
                .map(|(_, label)| MenuRow {
                    text: label.clone(),
                    divider: false,
                    destructive: false,
                })
                .collect();
            // Beside the main popup, first row aligned with the "Move to tab" row.
            let sub_anchor = (mrect.right() + 1, mrect.y.saturating_sub(1));
            let sub_rects = render_popup(f, area, sub_anchor, &sub_rows, app.hover, app.compact, t);
            if let Some(menu) = app.pane_menu.as_mut() {
                menu.tab_rects = move_targets
                    .iter()
                    .map(|(tg, _)| *tg)
                    .zip(sub_rects)
                    .collect();
            }
        }
        _ => {
            if let Some(menu) = app.pane_menu.as_mut() {
                menu.tab_rects.clear();
            }
        }
    }
}

pub(super) fn draw_agent_menu(
    f: &mut RenderTarget,
    area: Rect,
    app: &mut App,
    cat: &Catalog,
    t: &Theme,
) {
    let Some(menu) = app.agent_menu.as_ref() else {
        return;
    };
    let anchor = menu.anchor;
    let items = app.agent_menu_items(menu.target);
    let extras = menu.module_actions.clone();
    let rows: Vec<MenuRow> = items
        .iter()
        .map(|it| MenuRow {
            text: agent_label(*it, cat, &extras),
            divider: matches!(it, AgentMenuItem::Divider),
            destructive: matches!(it, AgentMenuItem::Close),
        })
        .collect();
    let rects = render_popup(f, area, anchor, &rows, app.hover, app.compact, t);
    if let Some(menu) = app.agent_menu.as_mut() {
        menu.items = items.into_iter().zip(rects).collect();
    }
}

fn agent_label(it: AgentMenuItem, cat: &Catalog, extras: &[ModuleMenuAction]) -> String {
    match it {
        AgentMenuItem::Resume => cat.menu_resume.to_string(),
        AgentMenuItem::RenamePane => cat.menu_rename.to_string(),
        AgentMenuItem::Pin => cat.menu_pin.to_string(),
        AgentMenuItem::Unpin => cat.menu_unpin.to_string(),
        AgentMenuItem::Close => cap_first(cat.act_close),
        AgentMenuItem::Divider => String::new(),
        AgentMenuItem::Module(i) => module_label(extras, i),
    }
}

fn ws_label(it: WsMenuItem, cat: &Catalog, extras: &[ModuleMenuAction]) -> String {
    match it {
        WsMenuItem::Pin => cat.menu_pin.to_string(),
        WsMenuItem::Unpin => cat.menu_unpin.to_string(),
        WsMenuItem::Close => cap_first(cat.act_close),
        WsMenuItem::Rename => cat.menu_rename.to_string(),
        WsMenuItem::DeleteWorktree => cat.menu_delete_worktree.to_string(),
        WsMenuItem::NewWorktree => cat.menu_new_worktree.to_string(),
        WsMenuItem::OpenWorktree => cat.menu_open_worktree.to_string(),
        WsMenuItem::Divider => String::new(),
        WsMenuItem::OpenGit => cat.menu_open_git.to_string(),
        WsMenuItem::OpenOrch => cat.menu_open_board.to_string(),
        WsMenuItem::OpenMission => cat.mc_open.to_string(),
        WsMenuItem::Module(i) => module_label(extras, i),
    }
}

fn pane_label(it: PaneMenuItem, cat: &Catalog, extras: &[ModuleMenuAction]) -> String {
    match it {
        PaneMenuItem::SplitVertical => cat.menu_split_vertical.to_string(),
        PaneMenuItem::SplitHorizontal => cat.menu_split_horizontal.to_string(),
        PaneMenuItem::ForkPane => cat.menu_fork_pane.to_string(),
        PaneMenuItem::OpenLink => cat.menu_open_link.to_string(),
        PaneMenuItem::OpenFile => cat.menu_open_file.to_string(),
        PaneMenuItem::RunningCmd => cat.menu_running_cmd.to_string(),
        PaneMenuItem::RenamePane => cat.menu_rename.to_string(),
        // A trailing ▸ marks the row that opens the tabs submenu.
        PaneMenuItem::MoveToTab => format!("{} ▸", cat.menu_move_to_tab),
        PaneMenuItem::Divider => String::new(),
        PaneMenuItem::Close => cap_first(cat.act_close),
        PaneMenuItem::Module(i) => module_label(extras, i),
    }
}

fn tab_label(it: TabMenuItem, cat: &Catalog, extras: &[ModuleMenuAction]) -> String {
    match it {
        TabMenuItem::Rename => cat.menu_rename.to_string(),
        TabMenuItem::MoveLeft => format!("{} {}", cap_first(cat.act_move), cat.side_left),
        TabMenuItem::MoveRight => format!("{} {}", cap_first(cat.act_move), cat.side_right),
        TabMenuItem::SwapWith => format!("{} ▸", cat.tab_swap_with),
        TabMenuItem::Divider => String::new(),
        TabMenuItem::Module(i) => module_label(extras, i),
    }
}

/// A module action's row label. Module titles come from the module author, so
/// they are never translated — and a stale index renders blank rather than
/// panicking (the registry can change while a menu is open).
fn module_label(extras: &[ModuleMenuAction], i: usize) -> String {
    extras.get(i).map(|a| a.title.clone()).unwrap_or_default()
}

/// Uppercase the first character (no-op for scripts without case, e.g. CJK), so
/// the reused lower-case `act_close` reads as a menu label.
fn cap_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

pub(super) fn draw_file_menu(f: &mut RenderTarget, area: Rect, app: &mut App, t: &Theme) {
    let Some(menu) = app.file_menu.as_ref() else {
        return;
    };
    let anchor = menu.anchor;
    let editors = menu.editors.clone();
    let items = menu.build_items();
    let rows: Vec<MenuRow> = items
        .iter()
        .map(|it| MenuRow {
            text: file_label(*it, &editors),
            divider: matches!(it, FileMenuItem::Divider),
            destructive: matches!(it, FileMenuItem::Delete),
        })
        .collect();
    let rects = render_popup(f, area, anchor, &rows, app.hover, app.compact, t);
    if let Some(menu) = app.file_menu.as_mut() {
        menu.items = items.into_iter().zip(rects).collect();
    }
}

pub(super) fn draw_diff_menu(f: &mut RenderTarget, area: Rect, app: &mut App, t: &Theme) {
    let Some(menu) = app.diff_menu.as_ref() else {
        return;
    };
    let items = [
        DiffMenuItem::OpenPreview,
        DiffMenuItem::OpenPane,
        DiffMenuItem::OpenTab,
        DiffMenuItem::CopyPath,
    ];
    let rows: Vec<MenuRow> = items
        .iter()
        .map(|item| MenuRow {
            text: match item {
                DiffMenuItem::OpenPreview => "Open Preview",
                DiffMenuItem::OpenPane => "Open in Pane",
                DiffMenuItem::OpenTab => "Open in Tab",
                DiffMenuItem::CopyPath => "Copy Path",
            }
            .to_string(),
            divider: false,
            destructive: false,
        })
        .collect();
    let rects = render_popup(f, area, menu.anchor, &rows, app.hover, app.compact, t);
    if let Some(menu) = app.diff_menu.as_mut() {
        menu.items = items.into_iter().zip(rects).collect();
    }
}

/// FILES-menu labels are plain English (this menu is not localized — unlike the
/// workspace/pane menus — and editor names are proper nouns anyway).
fn file_label(it: FileMenuItem, editors: &[(String, String)]) -> String {
    match it {
        FileMenuItem::OpenReadonly => "Open in Tab".to_string(),
        FileMenuItem::OpenWith(i) => editors
            .get(i)
            .map(|(_, label)| format!("Open in {label}"))
            .unwrap_or_default(),
        FileMenuItem::NewFile => "New File".to_string(),
        FileMenuItem::NewFolder => "New Folder".to_string(),
        FileMenuItem::Rename => "Rename".to_string(),
        FileMenuItem::CopyPath => "Copy Path".to_string(),
        FileMenuItem::InsertPath => "Insert Path".to_string(),
        FileMenuItem::Divider => String::new(),
        FileMenuItem::Delete => "Delete".to_string(),
    }
}

/// The context menu a module declared for one of its dock rows (docs/52).
///
/// Unlike the other menus this renders a snapshot rather than recomputing its
/// items — the live dock rows may already have been replaced underneath it.
pub(super) fn draw_dock_menu(f: &mut RenderTarget, area: Rect, app: &mut App, t: &Theme) {
    let Some(menu) = app.dock_menu.as_ref() else {
        return;
    };
    let rows: Vec<MenuRow> = menu
        .items
        .iter()
        .map(|it| MenuRow {
            text: it.title.clone(),
            divider: it.is_divider(),
            destructive: it.destructive,
        })
        .collect();
    let anchor = menu.anchor;
    let rects = render_popup(f, area, anchor, &rows, app.hover, app.compact, t);
    if let Some(menu) = app.dock_menu.as_mut() {
        menu.rects = rects;
    }
}

#[cfg(test)]
mod label_case_tests {
    use super::*;
    use crate::app::{AgentMenuItem, FileMenuItem, PaneMenuItem, TabMenuItem, WsMenuItem};

    /// Words that stay lower-case inside a title, unless they lead it.
    const MINOR: [&str; 11] = [
        "a", "an", "the", "to", "in", "on", "of", "for", "and", "or", "with",
    ];

    #[test]
    fn mobile_menu_hover_covers_the_full_touch_row() {
        let row = Rect::new(4, 7, 20, 2);
        assert!(row_is_hovered(row, Some((5, 7))));
        assert!(row_is_hovered(row, Some((5, 8))));
        assert!(!row_is_hovered(row, Some((5, 9))));
        assert!(!row_is_hovered(row, Some((24, 8))));
    }

    /// Every context-menu row reads as **Title Case**: each word capitalized bar
    /// the short articles/prepositions, which never lead. Hyphenated parts count
    /// as words of their own ("Read-Only"), and trailing marks like the submenu
    /// `▸` are ignored.
    fn offending_word(label: &str) -> Option<String> {
        let mut lead = true;
        for word in label.split_whitespace() {
            let word = word.trim_matches(|c: char| !c.is_alphanumeric());
            if word.is_empty() {
                continue;
            }
            for part in word.split('-') {
                let Some(first) = part.chars().find(|c| c.is_alphabetic()) else {
                    continue;
                };
                let minor = MINOR.contains(&part.to_lowercase().as_str());
                if !first.is_uppercase() && (!minor || lead) {
                    return Some(part.to_string());
                }
                lead = false;
            }
        }
        None
    }

    /// The rule the check itself relies on — a guard against it silently passing
    /// everything (it would, if `offending_word` stopped looking at words).
    #[test]
    fn the_title_case_check_rejects_sentence_case() {
        assert_eq!(offending_word("Open Task Board"), None);
        assert_eq!(offending_word("Fork to New Pane"), None);
        assert_eq!(offending_word("Open in Tab"), None);
        assert_eq!(offending_word("Move to Tab ▸"), None);
        assert_eq!(offending_word("Open task board").as_deref(), Some("task"));
        assert_eq!(offending_word("Open (read-only)").as_deref(), Some("read"));
        assert_eq!(offending_word("to Open").as_deref(), Some("to"));
    }

    /// One casing standard across every context menu, so the workspace menu can't
    /// drift into "Open Mission Control" beside "Open task board" again.
    #[test]
    fn every_english_context_menu_row_is_title_case() {
        let cat = &crate::i18n::EN;
        let none: &[ModuleMenuAction] = &[];
        let editors = [("nvim".to_string(), "Neovim".to_string())];

        let mut rows: Vec<String> = Vec::new();
        for it in [
            WsMenuItem::Close,
            WsMenuItem::Rename,
            WsMenuItem::DeleteWorktree,
            WsMenuItem::NewWorktree,
            WsMenuItem::OpenWorktree,
            WsMenuItem::OpenGit,
            WsMenuItem::OpenOrch,
            WsMenuItem::OpenMission,
        ] {
            rows.push(ws_label(it, cat, none));
        }
        for it in PaneMenuItem::ALL.iter().copied() {
            rows.push(pane_label(it, cat, none));
        }
        for it in [
            TabMenuItem::Rename,
            TabMenuItem::MoveLeft,
            TabMenuItem::MoveRight,
            TabMenuItem::SwapWith,
        ] {
            rows.push(tab_label(it, cat, none));
        }
        // The "Move to Tab" submenu is part of the pane menu: its tab rows are
        // user content, but the trailing "New Tab" is ours (`move_targets` in
        // `app/mod.rs`).
        rows.push(cat.menu_new_tab.to_string());
        for it in [AgentMenuItem::Resume, AgentMenuItem::Close] {
            rows.push(agent_label(it, cat, none));
        }
        for it in [
            FileMenuItem::OpenReadonly,
            FileMenuItem::OpenWith(0),
            FileMenuItem::NewFile,
            FileMenuItem::NewFolder,
            FileMenuItem::Rename,
            FileMenuItem::CopyPath,
            FileMenuItem::InsertPath,
            FileMenuItem::Delete,
        ] {
            rows.push(file_label(it, &editors));
        }

        let bad: Vec<String> = rows
            .iter()
            .filter(|r| !r.is_empty())
            .filter_map(|r| offending_word(r).map(|w| format!("{r:?} (word {w:?})")))
            .collect();
        assert!(bad.is_empty(), "menu rows are not Title Case: {bad:#?}");
    }
}
