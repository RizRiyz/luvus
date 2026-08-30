use std::path::{Path, PathBuf};
use std::sync::Arc;

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use unicode_width::UnicodeWidthChar;

use crate::app::{App, Mode, Tab, ViewKind};
use crate::event::AppEvent;
use crate::files::preview::{
    self, DocumentView, LayoutKey, PreviewKind, PreviewLayout, PreviewLoad,
};
use crate::ids::PaneId;
use crate::layout::{Axis, TileLayout};

impl App {
    /// Open or focus an explicit preview from the Files dock. This always owns a
    /// complete tab and never consults or changes the normal file-open setting.
    pub fn open_document_preview_tab(&mut self, path: PathBuf, kind: PreviewKind) {
        if let Some(id) = self.preview_tab_showing(&path, kind) {
            self.focus_pane_global(id);
            return;
        }
        self.create_document_preview(path, kind, true);
    }

    /// Open or focus a sibling preview beside an authoritative file/editor pane.
    pub fn open_document_preview_pane(&mut self, path: PathBuf, kind: PreviewKind) {
        if let Some(id) = self.preview_in_active_tab(&path, kind) {
            self.layout_mut().focus = id;
            return;
        }
        self.create_document_preview(path, kind, false);
    }

    fn preview_tab_showing(&self, path: &Path, kind: PreviewKind) -> Option<PaneId> {
        self.ws().tabs.iter().find_map(|tab| {
            let leaves = tab.layout.leaves();
            let [id] = leaves.as_slice() else {
                return None;
            };
            matches!(
                self.views.get(id),
                Some(ViewKind::Preview(view)) if view.path == path && view.kind == kind
            )
            .then_some(*id)
        })
    }

    fn preview_in_active_tab(&self, path: &Path, kind: PreviewKind) -> Option<PaneId> {
        self.layout().leaves().into_iter().find(|id| {
            matches!(
                self.views.get(id),
                Some(ViewKind::Preview(view)) if view.path == path && view.kind == kind
            )
        })
    }

    fn create_document_preview(&mut self, path: PathBuf, kind: PreviewKind, tab: bool) {
        let id = PaneId::alloc();
        self.views
            .insert(id, ViewKind::Preview(DocumentView::new(path.clone(), kind)));
        if tab {
            let workspace = &mut self.workspaces[self.active_ws];
            workspace.tabs.push(Tab::panes(TileLayout::new(id)));
            workspace.active_tab = workspace.tabs.len() - 1;
        } else {
            self.layout_mut().split_focused(Axis::Col, id);
            self.layout_mut().focus = id;
        }
        self.schedule_preview_read(id, path);
        self.mode = Mode::Normal;
    }

    pub(crate) fn schedule_preview_read(&mut self, id: PaneId, path: PathBuf) {
        let Some(ViewKind::Preview(view)) = self.views.get_mut(&id) else {
            return;
        };
        view.read_token = view.read_token.wrapping_add(1);
        view.mtime = std::fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .ok();
        let token = view.read_token;
        let kind = view.kind;
        let tx = self.app_tx.clone();
        std::thread::spawn(move || {
            let load = preview::read(&path, kind);
            let _ = tx.send(AppEvent::PreviewRead {
                id,
                path,
                kind,
                token,
                load,
            });
        });
    }

    pub(crate) fn apply_preview_read(
        &mut self,
        id: PaneId,
        path: PathBuf,
        kind: PreviewKind,
        token: u64,
        load: PreviewLoad,
    ) -> bool {
        let Some(ViewKind::Preview(view)) = self.views.get_mut(&id) else {
            return false;
        };
        if view.read_token != token || view.path != path || view.kind != kind {
            return false;
        }
        view.apply(load);
        true
    }

    /// Called once geometry is known, before drawing. A missing width projection
    /// schedules exactly one transient worker; unchanged frames reuse the cache.
    pub(crate) fn ensure_preview_layouts(&mut self, rects: &[(PaneId, Rect)]) {
        let mut requests = Vec::new();
        for (id, rect) in rects {
            let key = LayoutKey {
                width: rect.width.max(1),
                ascii: false,
            };
            let Some(ViewKind::Preview(view)) = self.views.get_mut(id) else {
                continue;
            };
            if let Some(document) = view.begin_layout(key) {
                requests.push((
                    *id,
                    view.path.clone(),
                    view.kind,
                    view.read_token,
                    key,
                    document,
                ));
            }
        }
        for (id, path, kind, token, key, document) in requests {
            let tx = self.app_tx.clone();
            std::thread::spawn(move || {
                let layout = Arc::new(preview::layout::build(document, key));
                let _ = tx.send(AppEvent::PreviewLayout {
                    id,
                    path,
                    kind,
                    token,
                    key,
                    layout,
                });
            });
        }
    }

    pub(crate) fn apply_preview_layout(
        &mut self,
        id: PaneId,
        path: PathBuf,
        kind: PreviewKind,
        token: u64,
        key: LayoutKey,
        layout: Arc<PreviewLayout>,
    ) -> bool {
        let Some(ViewKind::Preview(view)) = self.views.get_mut(&id) else {
            return false;
        };
        if view.read_token != token || view.path != path || view.kind != kind {
            return false;
        }
        view.apply_layout(key, layout);
        true
    }

    pub fn handle_preview_key(&mut self, id: PaneId, key_event: KeyEvent) -> bool {
        let rect = self
            .pane_content_rects
            .iter()
            .find(|(pane, _)| *pane == id)
            .map(|(_, rect)| *rect);
        let layout_key = LayoutKey {
            width: rect.map_or(80, |rect| rect.width.max(1)),
            ascii: false,
        };
        let viewport = rect
            .map(|rect| rect.height.saturating_sub(1) as usize)
            .unwrap_or(20)
            .max(1);
        let Some(ViewKind::Preview(view)) = self.views.get_mut(&id) else {
            return false;
        };
        if view.search.as_ref().is_some_and(|search| search.editing) {
            match key_event.code {
                KeyCode::Char(ch) => view.search_push(ch),
                KeyCode::Backspace => view.search_backspace(),
                KeyCode::Enter => view.search_commit(layout_key, viewport),
                KeyCode::Esc => view.search_cancel(),
                _ => return false,
            }
            return true;
        }
        match key_event.code {
            KeyCode::Char('j') | KeyCode::Down => view.scroll_by(1, viewport, layout_key),
            KeyCode::Char('k') | KeyCode::Up => view.scroll_by(-1, viewport, layout_key),
            KeyCode::Char('d') => view.scroll_by(viewport as i32 / 2, viewport, layout_key),
            KeyCode::Char('u') => view.scroll_by(-(viewport as i32) / 2, viewport, layout_key),
            KeyCode::PageDown | KeyCode::Char(' ') => {
                view.scroll_by(viewport as i32, viewport, layout_key)
            }
            KeyCode::PageUp => view.scroll_by(-(viewport as i32), viewport, layout_key),
            KeyCode::Char('g') | KeyCode::Home => view.scroll = 0,
            KeyCode::Char('G') | KeyCode::End => view.goto_bottom(viewport, layout_key),
            KeyCode::Char('/') => view.search_begin(),
            KeyCode::Char('n') => view.search_step(true, viewport),
            KeyCode::Char('N') => view.search_step(false, viewport),
            KeyCode::Char('y') | KeyCode::Char('c') => {
                let text = view.document().map(|document| document.source.to_string());
                if let Some(text) = text {
                    self.pending_clipboard = Some(text);
                    let message = self.catalog.copied;
                    self.show_toast(message);
                } else {
                    self.show_toast("nothing to copy");
                }
                return true;
            }
            KeyCode::Char('q') => self.close_pane(id),
            KeyCode::Esc => {
                if view.search.is_some() {
                    view.search_cancel();
                } else {
                    self.close_pane(id);
                }
            }
            _ => return false,
        }
        true
    }

    /// Activate a rendered link only after an explicit modified click. Web
    /// targets use the existing client-side URL path; repository-relative links
    /// reopen through normal file behavior and never read during rendering.
    pub(crate) fn activate_preview_link(&mut self, pane: PaneId, target: String) {
        if target.starts_with("https://") || target.starts_with("http://") {
            self.open_url(target);
            return;
        }
        if target.starts_with('#') {
            self.show_toast("heading links are not available in preview yet");
            return;
        }
        let Some(ViewKind::Preview(view)) = self.views.get(&pane) else {
            return;
        };
        let preview_path = view.path.clone();
        let Some(workspace_root) = self
            .workspace_of_pane(pane)
            .map(|workspace| workspace.cwd.clone())
        else {
            return;
        };
        let target = target.split('#').next().unwrap_or_default();
        if target.is_empty() {
            return;
        }
        let candidate = PathBuf::from(target);
        if candidate.is_absolute() {
            self.show_toast("preview links must be relative to the workspace");
            return;
        }
        let unresolved = preview_path
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(candidate);
        let Ok(root) = workspace_root.canonicalize() else {
            self.show_toast("workspace path is unavailable");
            return;
        };
        let Ok(path) = unresolved.canonicalize() else {
            self.show_toast("linked file does not exist");
            return;
        };
        if !path.starts_with(&root) {
            self.show_toast("preview link is outside the workspace");
            return;
        }
        if path.is_file() {
            self.open_file_at(path, None);
        } else {
            self.show_toast("preview link is not a file");
        }
    }
}

/// Extract rendered preview text for the generic pane-selection path.
pub(crate) fn selection_text(
    view: &DocumentView,
    content: Rect,
    ((sx, sy), (ex, ey)): ((u16, u16), (u16, u16)),
    mobile: bool,
) -> Option<String> {
    let key = LayoutKey {
        width: content.width.max(1),
        ascii: false,
    };
    let layout = view.layout(key)?;
    let show_footer = !mobile || view.search.is_some();
    let body_bottom = content
        .bottom()
        .saturating_sub(u16::from(show_footer))
        .saturating_sub(1);
    let start_y = sy.clamp(content.y, body_bottom);
    let end_y = ey.clamp(content.y, body_bottom);
    let mut output = String::new();
    let mut first = true;
    for screen_y in start_y..=end_y {
        let row_index = view.scroll + usize::from(screen_y.saturating_sub(content.y));
        let rendered = layout.rows.get(row_index)?;
        let row = rendered.plain_text();
        let left = if screen_y == start_y {
            usize::from(sx.saturating_sub(content.x))
        } else {
            0
        };
        let right = if screen_y == end_y {
            usize::from(ex.saturating_sub(content.x)) + 1
        } else {
            usize::from(content.width)
        };
        if !first {
            match rendered.soft_wrap_spaces {
                Some(spaces) => output.push_str(&" ".repeat(spaces as usize)),
                None => output.push('\n'),
            }
        }
        first = false;
        output.push_str(slice_columns(&row, left, right).trim_end());
    }
    let output = output.trim_end_matches('\n').to_string();
    (!output.is_empty()).then_some(output)
}

/// Return only the currently rendered document body for double-click token
/// lookup. A preview has no terminal grid, so this provides the same cell-row
/// projection without including its status footer.
pub(crate) fn token_rows(view: &DocumentView, content: Rect, mobile: bool) -> Option<Vec<String>> {
    let key = LayoutKey {
        width: content.width.max(1),
        ascii: false,
    };
    let layout = view.layout(key)?;
    let show_footer = !mobile || view.search.is_some();
    let body_rows = content.height.saturating_sub(u16::from(show_footer)) as usize;
    Some(
        layout
            .rows
            .iter()
            .skip(view.scroll)
            .take(body_rows)
            .map(|row| row.plain_text())
            .collect(),
    )
}

fn slice_columns(text: &str, start: usize, end: usize) -> String {
    let mut column = 0usize;
    let mut output = String::new();
    for ch in text.chars() {
        let width = ch.width().unwrap_or(0);
        let next = column + width;
        if next > start && column < end {
            output.push(ch);
        }
        column = next;
        if column >= end {
            break;
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::Terminal;
    use std::sync::mpsc;

    #[test]
    fn explicit_preview_reuses_the_same_tab_without_changing_file_opening() {
        let _env = crate::persist::test_env("document-preview-tab");
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(100, 30, tx).unwrap();
        let path = PathBuf::from("README.md");
        let setting = app.config.layout.file_open.clone();
        app.open_document_preview_tab(path.clone(), PreviewKind::Markdown);
        let id = app.layout().focus;
        app.open_document_preview_tab(path, PreviewKind::Markdown);
        assert_eq!(app.layout().focus, id);
        assert_eq!(app.config.layout.file_open, setting);
        assert!(matches!(app.views.get(&id), Some(ViewKind::Preview(_))));
    }

    #[test]
    fn tracked_markdown_pane_offers_and_opens_a_sibling_preview() {
        let _env = crate::persist::test_env("document-preview-pane-menu");
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(100, 30, tx).unwrap();
        let source = app.layout().focus;
        let path = PathBuf::from("README.md");
        app.views.insert(
            source,
            ViewKind::File(crate::files::FileView::new(path.clone())),
        );
        app.open_pane_menu(source, 2, 2);
        assert!(app
            .pane_menu_items()
            .contains(&crate::app::PaneMenuItem::OpenMarkdownPreview));
        app.pane_menu_action(crate::app::PaneMenuItem::OpenMarkdownPreview);
        let preview = app.layout().focus;
        assert_ne!(preview, source);
        assert!(matches!(
            app.views.get(&preview),
            Some(ViewKind::Preview(view))
                if view.path == path && view.kind == PreviewKind::Markdown
        ));
        assert!(matches!(app.views.get(&source), Some(ViewKind::File(_))));
    }

    #[test]
    fn preview_events_reject_stale_path_kind_and_generation() {
        let _env = crate::persist::test_env("document-preview-stale");
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(100, 30, tx).unwrap();
        app.open_document_preview_tab(PathBuf::from("README.md"), PreviewKind::Markdown);
        let id = app.layout().focus;
        let token = match app.views.get(&id) {
            Some(ViewKind::Preview(view)) => view.read_token,
            _ => panic!("preview view"),
        };
        assert!(!app.apply_preview_read(
            id,
            PathBuf::from("other.md"),
            PreviewKind::Markdown,
            token,
            PreviewLoad::Error("stale".into()),
        ));
        assert!(!app.apply_preview_read(
            id,
            PathBuf::from("README.md"),
            PreviewKind::Mermaid,
            token,
            PreviewLoad::Error("stale".into()),
        ));
        assert!(!app.apply_preview_read(
            id,
            PathBuf::from("README.md"),
            PreviewKind::Markdown,
            token.wrapping_add(1),
            PreviewLoad::Error("stale".into()),
        ));
        assert!(matches!(
            app.views.get(&id),
            Some(ViewKind::Preview(view)) if matches!(view.load, PreviewLoad::Loading)
        ));
    }

    #[test]
    fn document_preview_identity_survives_session_restore() {
        let _env = crate::persist::test_env("document-preview-restore");
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(100, 30, tx).unwrap();
        let path = PathBuf::from("docs/guide.mermaid");
        app.open_document_preview_tab(path.clone(), PreviewKind::Mermaid);
        let id = app.layout().focus;
        if let Some(ViewKind::Preview(view)) = app.views.get_mut(&id) {
            view.scroll = 7;
        }
        let snapshot = crate::persist::snapshot(&app);
        let (restore_tx, _restore_rx) = mpsc::channel();
        let restored = App::from_snapshot(snapshot, restore_tx).expect("restore preview");
        assert!(restored.views.values().any(|view| matches!(
            view,
            ViewKind::Preview(preview)
                if preview.path == path
                    && preview.kind == PreviewKind::Mermaid
                    && preview.scroll == 7
        )));
    }

    #[test]
    fn rendered_links_require_an_explicit_modified_click() {
        let _env = crate::persist::test_env("document-preview-link");
        let root = std::env::temp_dir().join(format!("luvus-preview-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let readme = root.join("README.md");
        let guide = root.join("guide.md");
        std::fs::write(&readme, "# Home\n\n[Open the guide](guide.md)\n").unwrap();
        std::fs::write(&guide, "# Guide\n").unwrap();

        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(100, 30, tx).unwrap();
        app.workspaces[app.active_ws].cwd = root.clone();
        app.open_document_preview_tab(readme.clone(), PreviewKind::Markdown);
        let preview_id = app.layout().focus;
        let load = preview::read(&readme, PreviewKind::Markdown);
        let document = match &load {
            PreviewLoad::Ready(document) => Arc::clone(document),
            _ => panic!("markdown loaded"),
        };
        if let Some(ViewKind::Preview(view)) = app.views.get_mut(&preview_id) {
            view.apply(load);
        }

        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();
        let content = app
            .pane_content_rects
            .iter()
            .find(|(pane, _)| *pane == preview_id)
            .map(|(_, rect)| *rect)
            .unwrap();
        let key = LayoutKey {
            width: content.width,
            ascii: false,
        };
        let layout = Arc::new(preview::layout::build(document, key));
        if let Some(ViewKind::Preview(view)) = app.views.get_mut(&preview_id) {
            view.apply_layout(key, layout);
        }
        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();
        let (_, _, rect) = app
            .preview_link_rects
            .iter()
            .find(|(pane, target, _)| *pane == preview_id && target == "guide.md")
            .cloned()
            .expect("rendered link hit target");

        app.handle_event(AppEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: KeyModifiers::CONTROL,
        }));
        let opened: Vec<_> = app
            .views
            .values()
            .filter_map(|view| match view {
                ViewKind::File(view) => Some(view.path.clone()),
                _ => None,
            })
            .collect();
        let guide = guide.canonicalize().unwrap();
        assert!(
            opened.contains(&guide),
            "modified click opened {opened:?}, expected {guide:?}"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn preview_links_use_shared_url_validation_and_stay_in_workspace() {
        let _env = crate::persist::test_env("document-preview-link-boundary");
        let temp = std::env::temp_dir();
        let root = temp.join(format!("luvus-preview-root-{}", std::process::id()));
        let outside = temp.join(format!("luvus-preview-outside-{}.md", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_file(&outside);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("README.md"), "# Home\n").unwrap();
        std::fs::write(&outside, "# Outside\n").unwrap();

        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(100, 30, tx).unwrap();
        app.workspaces[app.active_ws].cwd = root.clone();
        app.open_document_preview_tab(root.join("README.md"), PreviewKind::Markdown);
        let pane = app.layout().focus;

        app.activate_preview_link(pane, "http://".into());
        assert!(app.pending_open_url.is_none());
        app.activate_preview_link(pane, "https://example.com/docs".into());
        assert_eq!(
            app.pending_open_url.as_deref(),
            Some("https://example.com/docs")
        );

        app.activate_preview_link(
            pane,
            format!("../{}", outside.file_name().unwrap().to_string_lossy()),
        );
        assert!(!app.views.values().any(|view| matches!(
            view,
            ViewKind::File(file) if file.path == outside
        )));

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_file(outside);
    }

    #[test]
    fn mobile_selection_uses_the_footer_free_body_height() {
        let document = Arc::new(crate::files::preview::PreviewDocument::new(
            Arc::<str>::from("a\nb\nc"),
            vec![crate::files::preview::Block::Code {
                language: None,
                text: "a\nb\nc".into(),
                range: 0..5,
            }],
        ));
        let mut view = DocumentView::new(PathBuf::from("README.md"), PreviewKind::Markdown);
        view.apply(PreviewLoad::Ready(Arc::clone(&document)));
        let key = LayoutKey {
            width: 10,
            ascii: false,
        };
        view.apply_layout(
            key,
            Arc::new(crate::files::preview::layout::build(document, key)),
        );
        let content = Rect::new(0, 0, 10, 3);

        assert_eq!(
            selection_text(&view, content, ((0, 2), (9, 2)), true).as_deref(),
            Some("  c")
        );
        assert_eq!(
            selection_text(&view, content, ((0, 2), (9, 2)), false).as_deref(),
            Some("  b")
        );
    }

    fn code_preview(text: &str, width: u16) -> DocumentView {
        let document = Arc::new(crate::files::preview::PreviewDocument::new(
            Arc::<str>::from(text),
            vec![crate::files::preview::Block::Code {
                language: None,
                text: text.into(),
                range: 0..text.len(),
            }],
        ));
        let mut view = DocumentView::new(PathBuf::from("sample.md"), PreviewKind::Markdown);
        view.apply(PreviewLoad::Ready(Arc::clone(&document)));
        let key = LayoutKey {
            width,
            ascii: false,
        };
        view.apply_layout(
            key,
            Arc::new(crate::files::preview::layout::build(document, key)),
        );
        view
    }

    #[test]
    fn wrapped_preview_selection_joins_visual_rows_without_a_newline() {
        let view = code_preview("alpha beta gamma", 10);
        let content = Rect::new(0, 0, 10, 3);

        assert_eq!(
            selection_text(&view, content, ((0, 0), (9, 1)), false).as_deref(),
            Some("  alpha beta gamma")
        );
    }

    #[test]
    fn preview_selection_preserves_real_document_line_breaks() {
        let view = code_preview("alpha\nbeta", 10);
        let content = Rect::new(0, 0, 10, 3);

        assert_eq!(
            selection_text(&view, content, ((0, 0), (9, 1)), false).as_deref(),
            Some("  alpha\n  beta")
        );
    }
}
