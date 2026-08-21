//! App-layer file-tree actions (docs/38 FILE-1): keeping the tree pointed at the
//! active node, scheduling directory reads off the loop, and opening a file.

use crate::files::view_text_w;
use std::path::{Path, PathBuf};

use ratatui::crossterm::event::{KeyCode, KeyEvent};

use crate::app::{
    App, DockKind, FileMenu, FileMenuItem, FilePrompt, FilePromptKind, Mode, Tab, ViewKind,
    FILE_NAME_MAX,
};
use crate::event::AppEvent;
use crate::files::FileView;
use crate::ids::PaneId;
use crate::layout::{Axis, TileLayout};

/// Where a file opens.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OpenTarget {
    /// A reused single-click preview pane (replaced as you click around).
    Preview,
    /// A new, permanent pane split beside the focus.
    Pane,
    /// A whole new tab.
    Tab,
}

impl App {
    /// Keep the FILES dock honest, off the render path. Called from `detect_tick`:
    /// re-roots the tree to the active node, then schedules a worker read for any
    /// directory that should be on screen but has not been read yet. Cheap when
    /// there is nothing to do (a few `HashSet` checks), and a no-op when the dock
    /// isn't mounted.
    pub fn ensure_file_tree(&mut self) {
        if self.sidebars.side_of(&DockKind::Files).is_none() {
            return;
        }
        let cwd = self.ws().cwd.clone();
        self.file_tree.set_root(cwd);
        self.load_pending_dirs();
        self.rescan_file_tree();
        self.refresh_git_status();
    }

    /// Re-read the directories currently on screen so files created or removed
    /// outside luvus (by an agent, a terminal command, another process) appear.
    /// Gated to ~1.5s and never descends into collapsed folders, so it stays
    /// cheap even on a big repo; `apply_dir` drops an unchanged listing, so a
    /// quiet tree costs one `read_dir` per open folder and no re-render.
    fn rescan_file_tree(&mut self) {
        if std::time::Instant::now().duration_since(self.last_file_scan_at)
            < std::time::Duration::from_millis(1500)
        {
            return;
        }
        self.last_file_scan_at = std::time::Instant::now();
        let dirs = self.file_tree.loaded_visible_dirs();
        if dirs.is_empty() {
            return;
        }
        let tx = self.app_tx.clone();
        std::thread::spawn(move || {
            for path in dirs {
                let entries = crate::files::read_dir_entries(&path);
                let _ = tx.send(AppEvent::DirRead { path, entries });
            }
        });
    }

    /// Refresh the FILES-dock git tint (docs/38 FILE-6) off the loop, at most
    /// every 2s and never piling up. `git status` can be slow on a huge repo, so
    /// it runs on a worker thread and posts `FileGitStatus` back.
    fn refresh_git_status(&mut self) {
        if self.git_status_inflight
            || std::time::Instant::now().duration_since(self.last_git_status_at)
                < std::time::Duration::from_secs(2)
        {
            return;
        }
        self.last_git_status_at = std::time::Instant::now();
        self.git_status_inflight = true;
        let root = self.file_tree.root().to_path_buf();
        let tx = self.app_tx.clone();
        std::thread::spawn(move || {
            let map = crate::git::local::tree_status(&root);
            let _ = tx.send(AppEvent::FileGitStatus(map));
        });
    }

    /// Resolve a possibly-relative path (from the API/CLI) against the active
    /// node's folder.
    pub fn resolve_file_path(&self, raw: &str) -> PathBuf {
        let p = PathBuf::from(raw);
        if p.is_absolute() {
            p
        } else {
            self.ws().cwd.join(p)
        }
    }

    /// Live refresh (docs/38 FILE-5): re-read any open file view whose file
    /// changed on disk since we last read it. One `stat` per open view, ~1s —
    /// cheap (there are rarely more than a couple). Called from `detect_tick`.
    pub fn ensure_file_views(&mut self) {
        if self.views.is_empty() {
            return;
        }
        let mut stale = Vec::new();
        for (id, ViewKind::File(v)) in self.views.iter() {
            let disk = std::fs::metadata(&v.path).and_then(|m| m.modified()).ok();
            if disk.is_some() && disk != v.mtime {
                stale.push((*id, v.path.clone(), disk));
            }
        }
        for (id, path, mtime) in stale {
            if let Some(ViewKind::File(v)) = self.views.get_mut(&id) {
                v.mtime = mtime; // record now so we don't reschedule until it changes again
            }
            self.schedule_file_read(id, path);
        }
    }

    /// `Ctrl+Space e`: mount the FILES dock on the left sidebar, or unmount it if
    /// it is already shown. Mounting also makes sure the sidebar is visible.
    pub fn toggle_files_dock(&mut self) {
        if self.sidebars.side_of(&DockKind::Files).is_some() {
            self.unmount_dock(&DockKind::Files);
        } else {
            self.sidebars.left.docks.push(DockKind::Files);
            self.sidebars.left.visible = true;
            self.save_sidebars();
        }
    }

    /// Toggle whether dotfiles show in the FILES tree, and remember the choice.
    /// `.git` stays hidden either way. No re-read: `visible_rows` re-flattens the
    /// already-cached listing when `show_hidden` flips (docs/38).
    pub fn toggle_files_hidden(&mut self) {
        let show = !self.file_tree.show_hidden;
        self.file_tree.show_hidden = show;
        self.file_tree.scroll = 0;
        self.config.layout.files_show_hidden = show;
        crate::config::save(&self.config);
    }

    /// A FILES row was clicked: expand/collapse a folder, or open a file in a
    /// **preview** pane (VS Code style — one reused pane while browsing).
    pub fn file_row_activate(&mut self, index: usize, target: OpenTarget) {
        let Some(row) = self.file_tree.visible_rows().get(index).cloned() else {
            return;
        };
        if row.is_dir {
            self.file_tree.toggle(&row.path);
            // Schedule the read *now* so an expand feels instant — don't wait for
            // the 1 Hz `ensure_file_tree` tick (that cadence is for background
            // re-root/refresh, not a user click).
            self.load_pending_dirs();
        } else if target == OpenTarget::Tab {
            // A plain click honors the configured default (read-only or an editor).
            self.open_file_at(row.path, None);
        } else {
            // Shift+click (Pane) / preview always opens the native read-only view.
            self.open_file_view(row.path, target);
        }
    }

    /// Open `path` in a new tab the way a plain FILES click does (docs/38): the
    /// configured default, read-only viewer or a terminal editor. Shared by the
    /// FILES tree and by `Ctrl`+clicking a path printed in a pane (docs/58).
    ///
    /// `line` scrolls the built-in viewer to that line. It survives the async read
    /// because `FileView::apply` keeps `scroll` and clamps it to the file's length.
    /// A configured *editor* opens at the top: the flag for "start at line N"
    /// differs per editor, and guessing it wrong is worse than not jumping.
    pub fn open_file_at(&mut self, path: PathBuf, line: Option<u32>) {
        match self.file_open_editor() {
            Some(cmd) => self.open_file_in_editor(path, &cmd),
            None => {
                self.open_file_view(path.clone(), OpenTarget::Tab);
                if let Some(l) = line {
                    if let Some(id) = self.view_showing(&path) {
                        if let Some(crate::app::ViewKind::File(v)) = self.views.get_mut(&id) {
                            v.scroll = l.saturating_sub(1) as usize;
                        }
                    }
                }
            }
        }
    }

    /// The configured default open action (docs/38), resolved to an editor
    /// run-command — or `None` for the read-only viewer. A configured editor
    /// that is no longer installed degrades to read-only, so a plain click never
    /// silently does nothing.
    fn file_open_editor(&self) -> Option<String> {
        let choice = self.config.layout.file_open.trim();
        if choice.is_empty() || choice == crate::config::FILE_OPEN_READONLY {
            return None;
        }
        self.editors
            .iter()
            .find(|(cmd, _)| cmd == choice)
            .map(|(cmd, _)| cmd.clone())
    }

    /// Open `path` in a terminal editor in a **new tab** (docs/38). `editor` is a
    /// run-command such as `"vim"` or `"emacs -nw"`; it runs as a real PTY pane
    /// (argv = the editor's words + the file path — a literal argument, so no
    /// shell quoting is involved), so quitting the editor fires `PtyExit` and the
    /// tab closes. The pane's cwd is the file's folder.
    pub fn open_file_in_editor(&mut self, path: PathBuf, editor: &str) {
        let cwd = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.ws().cwd.clone());
        let mut argv: Vec<String> = editor.split_whitespace().map(str::to_string).collect();
        if argv.is_empty() {
            return;
        }
        argv.push(path.to_string_lossy().into_owned());
        let id = PaneId::alloc();
        let history_budget_bytes = self.config.scrollback_bytes();
        match crate::terminal::pty::Pane::spawn_command(
            id,
            80,
            24,
            cwd,
            self.app_tx.clone(),
            &argv,
            &[],
            history_budget_bytes,
        ) {
            Ok(pane) => {
                let cmd = pane.command.clone();
                self.panes.insert(id, pane);
                self.status.insert(id, crate::app::PaneStatus::new(cmd));
                // Track the file so the tab bar labels this tab `■ name`, exactly
                // like a read-only view tab. An editor pane is an ordinary PTY
                // pane with no file view behind it, so without this the tab bar
                // has nothing to derive a name from and falls back to the number.
                self.editor_files.insert(id, path.clone());
                let ws = &mut self.workspaces[self.active_ws];
                ws.tabs.push(Tab::panes(TileLayout::new(id)));
                ws.active_tab = ws.tabs.len() - 1;
                self.zoomed = false;
                self.session_dirty = true;
                self.mode = Mode::Normal;
                self.emit_event(
                    "pane.created",
                    serde_json::json!({"pane": id.0.to_string()}),
                );
            }
            Err(e) => self.show_toast(format!("cannot open editor: {e}")),
        }
    }

    /// Schedule an off-loop `read_dir` for every directory that should be on
    /// screen but hasn't been read yet. Shared by the periodic `ensure_file_tree`
    /// and the immediate on-expand path so a click loads without a visible lag.
    fn load_pending_dirs(&mut self) {
        for path in self.file_tree.needs_load() {
            self.file_tree.mark_pending(path.clone());
            let tx = self.app_tx.clone();
            std::thread::spawn(move || {
                let entries = crate::files::read_dir_entries(&path);
                let _ = tx.send(AppEvent::DirRead { path, entries });
            });
        }
    }

    // ── FILES-dock right-click CRUD (docs/38 FILE-6) ─────────────────────────

    /// Open the file context menu for visible row `index`, anchored at the cursor.
    pub fn open_file_menu(&mut self, index: usize, col: u16, row: u16) {
        if let Some(r) = self.file_tree.visible_rows().get(index).cloned() {
            // Snapshot the editor list for a file (open actions are file-only), so
            // an `OpenWith(i)` picked from this menu maps to the same editor even
            // if the cache changes underneath it.
            let editors = if r.is_dir {
                Vec::new()
            } else {
                self.editors.clone()
            };
            self.file_menu = Some(FileMenu {
                path: r.path,
                is_dir: r.is_dir,
                anchor: (col, row),
                items: Vec::new(),
                editors,
            });
        }
    }

    /// A click inside the open file menu: run the hit item, else dismiss.
    pub fn file_menu_click(&mut self, col: u16, row: u16) {
        let hit = self.file_menu.as_ref().and_then(|m| {
            m.items
                .iter()
                .find(|(_, r)| col >= r.x && col < r.right() && row >= r.y && row < r.bottom())
                .map(|(it, _)| *it)
        });
        match hit {
            Some(FileMenuItem::Divider) => {}
            Some(it) => self.file_menu_action(it),
            None => self.file_menu = None,
        }
    }

    #[cfg(test)]
    pub fn file_menu_action_pub(&mut self, item: FileMenuItem) {
        self.file_menu_action(item);
    }
    fn file_menu_action(&mut self, item: FileMenuItem) {
        let Some(menu) = self.file_menu.take() else {
            return;
        };
        // New entries land *inside* a folder, or beside a clicked file.
        let dir = if menu.is_dir {
            menu.path.clone()
        } else {
            menu.path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| menu.path.clone())
        };
        let prompt = |kind, dir, target, buffer| {
            Some(FilePrompt {
                kind,
                dir,
                target,
                buffer,
                error: None,
            })
        };
        match item {
            // Open actions target the clicked file (never a folder).
            FileMenuItem::OpenReadonly => self.open_file_view(menu.path.clone(), OpenTarget::Tab),
            FileMenuItem::OpenWith(i) => {
                if let Some((cmd, _)) = menu.editors.get(i).cloned() {
                    self.open_file_in_editor(menu.path.clone(), &cmd);
                }
            }
            FileMenuItem::NewFile => {
                self.file_prompt = prompt(FilePromptKind::NewFile, dir, None, String::new())
            }
            FileMenuItem::NewFolder => {
                self.file_prompt = prompt(FilePromptKind::NewFolder, dir, None, String::new())
            }
            FileMenuItem::Rename => {
                let parent = menu
                    .path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| menu.path.clone());
                let name = menu
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.file_prompt = prompt(
                    FilePromptKind::Rename,
                    parent,
                    Some(menu.path.clone()),
                    name,
                )
            }
            FileMenuItem::CopyPath => {
                self.pending_clipboard = Some(menu.path.to_string_lossy().into_owned());
                self.show_toast("copied path");
            }
            FileMenuItem::Delete => self.file_delete = Some(menu.path),
            FileMenuItem::Divider => {}
        }
    }

    /// Keys for the create/rename prompt: type the name, `⏎` commit, `Esc` cancel.
    pub fn file_prompt_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.file_prompt = None,
            KeyCode::Enter => self.commit_file_prompt(),
            KeyCode::Backspace => {
                if let Some(p) = self.file_prompt.as_mut() {
                    p.buffer.pop();
                    p.error = None;
                }
            }
            KeyCode::Char(c) => {
                if let Some(p) = self.file_prompt.as_mut() {
                    if p.buffer.chars().count() < FILE_NAME_MAX {
                        p.buffer.push(c);
                        p.error = None;
                    }
                }
            }
            _ => {}
        }
    }

    fn commit_file_prompt(&mut self) {
        let Some(p) = self.file_prompt.as_ref() else {
            return;
        };
        let name = p.buffer.trim().to_string();
        if name.is_empty() {
            return;
        }
        // No path separators or `..` — a name, not a path.
        if name.contains(['/', '\\']) || name == ".." || name == "." {
            if let Some(pr) = self.file_prompt.as_mut() {
                pr.error = Some("name can't contain a path".into());
            }
            return;
        }
        let dest = p.dir.join(&name);
        let (kind, target) = (p.kind, p.target.clone());
        let result = match kind {
            FilePromptKind::NewFile => {
                if dest.exists() {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "already exists",
                    ))
                } else {
                    std::fs::write(&dest, b"")
                }
            }
            FilePromptKind::NewFolder => std::fs::create_dir(&dest),
            FilePromptKind::Rename => std::fs::rename(target.as_ref().unwrap(), &dest),
        };
        match result {
            Ok(()) => {
                self.file_prompt = None;
                self.after_fs_change(&dest);
                self.show_toast(match kind {
                    FilePromptKind::Rename => "renamed",
                    _ => "created",
                });
            }
            Err(e) => {
                if let Some(pr) = self.file_prompt.as_mut() {
                    pr.error = Some(e.to_string());
                }
            }
        }
    }

    /// Keys for the delete-confirm modal: `y`/`⏎` delete, anything else cancels.
    pub fn file_delete_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.confirm_delete(),
            _ => self.file_delete = None,
        }
    }

    fn confirm_delete(&mut self) {
        let Some(path) = self.file_delete.take() else {
            return;
        };
        let result = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match result {
            Ok(()) => {
                self.after_fs_change(&path);
                self.show_toast("deleted");
            }
            Err(e) => self.show_toast(format!("delete failed: {e}")),
        }
    }

    /// After a create/rename/delete: re-read the tree, reveal the path, re-tint.
    fn after_fs_change(&mut self, path: &Path) {
        self.file_tree.invalidate();
        self.load_pending_dirs();
        self.file_tree.reveal(path);
        // Force a git re-tint on the next tick, not up to 2s later.
        self.last_git_status_at = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(10))
            .unwrap_or_else(std::time::Instant::now);
        self.refresh_git_status();
    }

    /// The leaf id of an open view already showing `path`, if any.
    fn view_showing(&self, path: &std::path::Path) -> Option<PaneId> {
        self.views
            .iter()
            .find_map(|(id, ViewKind::File(v))| (v.path == path).then_some(*id))
    }

    /// Open `path` in a native file view (docs/38 FILE-3). `Preview` reuses the
    /// one preview pane; `Pane` splits a fresh permanent pane; `Tab` opens a new
    /// tab. The file is read on a worker thread and applied via `FileRead`.
    pub fn open_file_view(&mut self, path: PathBuf, target: OpenTarget) {
        // Already open? Focus that view instead of opening a duplicate.
        if let Some(id) = self.view_showing(&path) {
            self.focus_pane_global(id);
            return;
        }
        // Reuse the live preview pane: just swap its content.
        if target == OpenTarget::Preview {
            if let Some(id) = self.preview_view.filter(|id| self.views.contains_key(id)) {
                self.set_view_file(id, path);
                self.focus_pane_global(id);
                return;
            }
        }

        let id = PaneId::alloc();
        self.views
            .insert(id, ViewKind::File(FileView::new(path.clone())));
        match target {
            OpenTarget::Tab => {
                let ws = &mut self.workspaces[self.active_ws];
                ws.tabs.push(Tab::panes(TileLayout::new(id)));
                ws.active_tab = ws.tabs.len() - 1;
            }
            OpenTarget::Preview | OpenTarget::Pane => {
                self.layout_mut().split_focused(Axis::Col, id);
                self.layout_mut().focus = id;
            }
        }
        if target == OpenTarget::Preview {
            self.preview_view = Some(id);
        }
        self.schedule_file_read(id, path);
        self.mode = Mode::Normal;
    }

    /// Point an existing view leaf at a different file and re-read it.
    fn set_view_file(&mut self, id: PaneId, path: PathBuf) {
        if let Some(ViewKind::File(v)) = self.views.get_mut(&id) {
            *v = FileView::new(path.clone());
        }
        self.schedule_file_read(id, path);
    }

    fn schedule_file_read(&mut self, id: PaneId, path: PathBuf) {
        // Record the mtime now so live refresh (FILE-5) only re-reads on a real
        // change, not immediately after this read.
        if let Some(ViewKind::File(v)) = self.views.get_mut(&id) {
            v.mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        }
        let tx = self.app_tx.clone();
        std::thread::spawn(move || {
            let load = crate::files::read_file(&path);
            let _ = tx.send(AppEvent::FileRead { id, load });
            // Change markers ride the same worker, *after* the text: the file
            // must render immediately even in a huge repo where `git diff` is
            // slow, and markers simply appear a moment later.
            let changes = crate::git::local::file_changes(&path);
            let _ = tx.send(AppEvent::FileChanges { id, changes });
        });
    }

    /// Copy the whole file to the clipboard, via the same mechanism as a pane
    /// text selection: queue `pending_clipboard` (the loop broadcasts it, the
    /// client writes the native clipboard + OSC 52) and flash a toast. Only
    /// text files copy; binary / too-large / errored views toast a reason.
    pub fn copy_file_view(&mut self, id: PaneId) {
        let text = match self.views.get(&id) {
            Some(ViewKind::File(v)) => match &v.load {
                crate::files::FileLoad::Text(lines) => Some(lines.join("\n")),
                _ => None,
            },
            None => return,
        };
        match text {
            Some(t) => {
                self.pending_clipboard = Some(t);
                let msg = self.catalog.copied;
                self.show_toast(msg);
            }
            None => self.show_toast("nothing to copy"),
        }
    }

    /// Keys for a focused file view: scroll, wrap, close. Returns whether the
    /// frame should repaint.
    pub fn handle_file_key(&mut self, id: PaneId, key: KeyEvent) -> bool {
        // Rows visible in the view = its pane content height minus the footer.
        let rect = self
            .pane_content_rects
            .iter()
            .find(|(pid, _)| *pid == id)
            .map(|(_, r)| *r);
        let viewport = rect
            .map(|r| r.height.saturating_sub(1) as usize)
            .unwrap_or(20);
        let Some(ViewKind::File(v)) = self.views.get_mut(&id) else {
            return false;
        };
        // Text column width: the scroll clamp needs it to measure how many rows a
        // soft-wrapped line really occupies.
        let text_w = rect.map(|r| view_text_w(v, r.width)).unwrap_or(0);
        // While typing a search query, keys edit the query.
        if v.search.as_ref().is_some_and(|s| s.editing) {
            match key.code {
                KeyCode::Char(c) => v.search_push(c),
                KeyCode::Backspace => v.search_backspace(),
                KeyCode::Enter => {
                    v.search_commit();
                    v.search_step(true, viewport); // reveal the first hit
                }
                KeyCode::Esc => v.search_cancel(),
                _ => return false,
            }
            return true;
        }
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => v.scroll_by(1, viewport, text_w),
            KeyCode::Char('k') | KeyCode::Up => v.scroll_by(-1, viewport, text_w),
            KeyCode::Char('d') => v.scroll_by(viewport as i32 / 2, viewport, text_w),
            KeyCode::Char('u') => v.scroll_by(-(viewport as i32) / 2, viewport, text_w),
            KeyCode::PageDown | KeyCode::Char(' ') => {
                v.scroll_by(viewport as i32, viewport, text_w)
            }
            KeyCode::PageUp => v.scroll_by(-(viewport as i32), viewport, text_w),
            KeyCode::Char('g') | KeyCode::Home => v.goto_top(),
            KeyCode::Char('G') | KeyCode::End => v.goto_bottom(viewport, text_w),
            KeyCode::Char('h') | KeyCode::Left => v.scroll_right(-8),
            KeyCode::Char('l') | KeyCode::Right => v.scroll_right(8),
            KeyCode::Char('w') => v.wrap = !v.wrap,
            KeyCode::Char('/') => v.search_begin(),
            KeyCode::Char('n') => v.search_step(true, viewport),
            KeyCode::Char('N') => v.search_step(false, viewport),
            // `y` copies the whole file to the clipboard, through the same path
            // as a pane text selection (native clipboard + OSC 52 + a toast).
            KeyCode::Char('y') | KeyCode::Char('c') => {
                self.copy_file_view(id);
                return true;
            }
            KeyCode::Char('q') => self.close_pane(id),
            KeyCode::Esc => {
                // Esc clears a committed search first, else closes the view.
                if v.search.is_some() {
                    v.search_cancel();
                } else {
                    self.close_pane(id);
                }
            }
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{DockKind, FileMenu, FileMenuItem};
    use ratatui::{backend::TestBackend, Terminal};

    /// A file's context menu leads with open actions (read-only + one per detected
    /// editor); a folder's does not — you don't "open" a directory into an editor.
    #[test]
    fn file_menu_offers_open_actions_for_files_only() {
        let editors = vec![
            ("vim".to_string(), "vim".to_string()),
            ("nano".to_string(), "nano".to_string()),
        ];
        let file = FileMenu {
            path: PathBuf::from("/tmp/x.rs"),
            is_dir: false,
            anchor: (0, 0),
            items: Vec::new(),
            editors: editors.clone(),
        };
        let items = file.build_items();
        assert!(
            items.contains(&FileMenuItem::OpenReadonly),
            "read-only offered"
        );
        assert!(
            items.contains(&FileMenuItem::OpenWith(0))
                && items.contains(&FileMenuItem::OpenWith(1)),
            "one open-in row per detected editor"
        );
        // The open block sits above the CRUD block (a divider between).
        let ro = items.iter().position(|i| *i == FileMenuItem::OpenReadonly);
        let del = items.iter().position(|i| *i == FileMenuItem::Delete);
        assert!(ro < del, "open actions come before Delete");

        let folder = FileMenu {
            path: PathBuf::from("/tmp"),
            is_dir: true,
            anchor: (0, 0),
            items: Vec::new(),
            editors: Vec::new(),
        };
        let ditems = folder.build_items();
        assert!(
            !ditems
                .iter()
                .any(|i| matches!(i, FileMenuItem::OpenReadonly | FileMenuItem::OpenWith(_))),
            "a folder gets no open actions"
        );
        assert!(
            ditems.contains(&FileMenuItem::NewFile),
            "folder still has CRUD"
        );
    }

    /// Opening a file with an editor spawns a real PTY pane (not a native view
    /// leaf) in a fresh tab, so it behaves like any other terminal program.
    #[test]
    fn opening_with_editor_spawns_a_pty_pane_in_a_new_tab() {
        let _env = crate::persist::test_env("file-editor-open");
        let dir = std::env::temp_dir().join(format!("luvus-ed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("edit.rs");
        std::fs::write(&file, b"x\n").unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        let tabs_before = app.workspaces[app.active_ws].tabs.len();

        // A real program launched with the file as a literal argv element, so
        // the pane is a true PTY: `cat` on unix, `cmd /c type` on Windows.
        let editor = if cfg!(windows) { "cmd /c type" } else { "cat" };
        app.open_file_in_editor(file.clone(), editor);

        assert_eq!(
            app.workspaces[app.active_ws].tabs.len(),
            tabs_before + 1,
            "a new tab opened for the editor"
        );
        let focus = app.layout().focus;
        assert!(
            app.panes.contains_key(&focus),
            "the editor runs in a real PTY pane"
        );
        assert!(
            !app.views.contains_key(&focus),
            "it is a terminal pane, not a read-only view leaf"
        );
        // The tab must show the file, exactly like the read-only viewer's tab:
        // the pane is tracked in `editor_files`, which is what makes the tab bar
        // render the same `■ name` label instead of the bare tab number.
        assert_eq!(
            app.editor_files.get(&focus).map(|p| p.as_path()),
            Some(file.as_path()),
            "the editor pane is tracked with its file for the tab label"
        );
        let ws = &app.workspaces[app.active_ws];
        assert!(
            ws.tabs[ws.active_tab].name.is_none(),
            "the label is derived live, not baked into a persisted tab name"
        );
        // Closing the pane untracks it, so the label can never outlive the editor.
        app.close_pane(focus);
        assert!(
            !app.editor_files.contains_key(&focus),
            "closing the editor pane untracks its file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The default-open resolver: `readonly` → None; a configured editor that is
    /// installed → its command; one that vanished → None (degrade to read-only).
    #[test]
    fn default_open_resolves_and_degrades() {
        let _env = crate::persist::test_env("file-open-default");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();

        assert_eq!(app.file_open_editor(), None, "default is read-only");

        // Configured for vim, but nothing is on PATH → falls back to read-only.
        app.config.layout.file_open = "vim".to_string();
        app.editors = Vec::new();
        assert_eq!(
            app.file_open_editor(),
            None,
            "an uninstalled configured editor degrades to read-only"
        );

        // Present on PATH → returned as the editor command.
        app.editors = vec![("vim".to_string(), "vim".to_string())];
        assert_eq!(app.file_open_editor().as_deref(), Some("vim"));
    }

    fn buffer_text(term: &Terminal<TestBackend>) -> String {
        let buf = term.backend().buffer();
        (0..buf.area.height)
            .map(|r| {
                (0..buf.area.width)
                    .map(|c| buf.cell((c, r)).map(|x| x.symbol()).unwrap_or(" "))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The dock renders the tree, and a click on a folder row expands it in place.
    #[test]
    fn files_dock_renders_and_a_click_expands() {
        let _env = crate::persist::test_env("files-dock-render");
        // A tiny real tree on disk.
        let root = std::env::temp_dir().join(format!("luvus-ft-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/mod.rs"), b"// hi").unwrap();
        std::fs::write(root.join("README.md"), b"# hi").unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        app.workspaces[app.active_ws].cwd = root.clone();
        app.sidebars.left.docks.push(DockKind::Files);

        // `ensure_file_tree` re-roots + schedules reads on worker threads; apply
        // the root read synchronously so the test is deterministic.
        app.ensure_file_tree();
        app.file_tree
            .apply_dir(root.clone(), crate::files::read_dir_entries(&root));

        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let text = buffer_text(&term);
        assert!(text.contains("FILES"), "header drawn");
        assert!(text.contains("src"), "a folder row drawn");
        assert!(text.contains("README.md"), "a file row drawn");
        // Collapsed: src's child is not visible yet.
        assert!(!text.contains("mod.rs"), "child hidden while collapsed");

        // Click the `src` row (find its rect) and re-render.
        let (idx, rect) = app
            .file_tree_rects
            .iter()
            .find(|(i, _)| app.file_tree.visible_rows()[*i].name == "src")
            .cloned()
            .expect("src row has a rect");
        assert!(app.file_tree.visible_rows()[idx].is_dir);
        app.file_row_activate(idx, OpenTarget::Preview);
        // The expand scheduled a read; apply it and re-render.
        app.file_tree.apply_dir(
            root.join("src"),
            crate::files::read_dir_entries(&root.join("src")),
        );
        let _ = rect;
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let text = buffer_text(&term);
        assert!(text.contains("mod.rs"), "child visible after expanding src");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Files get a small dot where folders get their chevron, so a file reads as
    /// a leaf instead of a blank gap. The alignment assertion is the point: all
    /// three glyphs must be one cell wide, or every file name shifts out of line
    /// with the folder names above it.
    #[test]
    fn files_get_a_dot_marker_aligned_with_folder_chevrons() {
        let _env = crate::persist::test_env("files-dot-marker");
        let root = std::env::temp_dir().join(format!("luvus-fdm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("README.md"), b"# hi").unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        app.workspaces[app.active_ws].cwd = root.clone();
        app.sidebars.left.docks.push(DockKind::Files);
        app.ensure_file_tree();
        app.file_tree
            .apply_dir(root.clone(), crate::files::read_dir_entries(&root));

        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let text = buffer_text(&term);

        let file_row = text
            .lines()
            .find(|l| l.contains("README.md"))
            .expect("file row drawn");
        let dir_row = text
            .lines()
            .find(|l| l.contains("src") && l.contains('▸'))
            .expect("folder row drawn");
        assert!(
            file_row.contains('•'),
            "file row carries the dot: {file_row:?}"
        );

        // Both names must start in the same column.
        let name_col = |line: &str, name: &str| line.find(name).expect("name on row");
        assert_eq!(
            name_col(file_row, "README.md"),
            name_col(dir_row, "src"),
            "file names line up with folder names\n  file: {file_row:?}\n  dir:  {dir_row:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Dotfiles show by default; the Settings → General toggle hides them and
    /// persists that choice, while `.git` stays hidden either way. Regression:
    /// `show_hidden` existed but was unreachable (no keybinding, menu, config, or
    /// button set it), so it was stuck off with no way to change it (docs/38).
    #[test]
    fn files_show_hidden_defaults_on_and_the_setting_persists() {
        let _env = crate::persist::test_env("files-hidden-toggle");
        let root = std::env::temp_dir().join(format!("luvus-fth-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".env"), b"X=1").unwrap();
        std::fs::write(root.join("main.rs"), b"fn main(){}").unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        app.workspaces[app.active_ws].cwd = root.clone();
        app.sidebars.left.docks.push(DockKind::Files);
        app.ensure_file_tree();
        app.file_tree
            .apply_dir(root.clone(), crate::files::read_dir_entries(&root));

        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        // Default on: the dotfile shows, `.git` never does.
        let text = buffer_text(&term);
        assert!(text.contains(".env"), "dotfile shown by default");
        assert!(!text.contains(".git"), ".git stays hidden regardless");

        // Flip the Settings → General "Show hidden files" row (what the toggle
        // does under the hood): dotfiles hide, live, without a re-read.
        app.toggle_files_hidden();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert!(
            !buffer_text(&term).contains(".env"),
            "dotfile hidden after turning the setting off"
        );

        // The choice persists, so a fresh App reads it back off.
        assert!(!app.config.layout.files_show_hidden);
        let reopened = App::new(120, 40, std::sync::mpsc::channel().0).unwrap();
        assert!(
            !reopened.file_tree.show_hidden,
            "a restart keeps the toggled-off choice"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The "Show hidden files" row appears in Settings → General, above the
    /// notification section, so the divider index stays correct.
    #[test]
    fn general_tab_has_show_hidden_above_the_notify_divider() {
        let _env = crate::persist::test_env("general-show-hidden-row");
        let (tx, _rx) = std::sync::mpsc::channel();
        let app = App::new(80, 24, tx).unwrap();
        let rows = app.general_rows();
        let pos = rows
            .iter()
            .position(|r| *r == crate::app::GeneralRow::FilesShowHidden)
            .expect("show-hidden row present");
        assert!(
            pos < app.general_section_start(),
            "it is a general setting, above the Notify divider"
        );
    }

    /// Opening a file makes a native view leaf that renders the file's contents
    /// and line numbers in a pane, scrolls, and closes with `q`.
    #[test]
    fn file_view_pane_renders_scrolls_and_closes() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let _env = crate::persist::test_env("file-view-pane");

        let dir = std::env::temp_dir().join(format!("luvus-fvp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("code.rs");
        let body: String = (1..=80).map(|i| format!("line number {i}\n")).collect();
        std::fs::write(&file, body).unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();

        // Open it in a permanent pane; apply the read synchronously.
        app.open_file_view(file.clone(), OpenTarget::Pane);
        let vid = app.layout().focus;
        assert!(
            app.views.contains_key(&vid),
            "a view leaf exists and is focused"
        );
        if let Some(ViewKind::File(v)) = app.views.get_mut(&vid) {
            v.apply(crate::files::read_file(&file));
        }

        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let text = buffer_text(&term);
        assert!(text.contains("code.rs"), "the pane title shows the file");
        assert!(text.contains("line number 1"), "first line rendered");
        assert!(text.contains("80 lines"), "footer line count");
        assert!(!text.contains("line number 80"), "bottom not visible yet");

        // Scroll to the bottom via the key path, then it shows.
        app.handle_file_key(vid, KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE));
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert!(
            buffer_text(&term).contains("line number 80"),
            "scrolled to end"
        );

        // `q` closes the view leaf; the tile collapses back to the shell.
        app.handle_file_key(vid, KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(!app.views.contains_key(&vid), "view leaf closed");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Live refresh: editing a file on disk re-reads the open view (FILE-5).
    #[test]
    fn open_view_live_refreshes_on_disk_change() {
        let _env = crate::persist::test_env("file-live-refresh");
        let dir = std::env::temp_dir().join(format!("luvus-lr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("live.txt");
        std::fs::write(&file, b"before\n").unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        app.open_file_view(file.clone(), OpenTarget::Pane);
        let vid = app.layout().focus;
        // Wait for the *text* to land. Each scheduled read sends two events —
        // `FileRead`, then `FileChanges` once `git diff` returns — so simply
        // taking the next event races: whichever the worker happens to have
        // queued first wins. Pump until the one we need arrives.
        pump_until_file_read(&rx, &mut app, vid);
        assert_eq!(
            app.views.get(&vid).map(|ViewKind::File(v)| v.line_count()),
            Some(1),
            "initial content is one line"
        );

        // Change the file with a strictly newer mtime, then tick.
        std::fs::write(&file, b"after edit\nsecond line\n").unwrap();
        filetime_set(&file, std::time::SystemTime::now());
        app.ensure_file_views();
        // A re-read was scheduled; apply events until its text arrives — the
        // first read's trailing `FileChanges` may still be queued ahead of it.
        pump_until_file_read(&rx, &mut app, vid);
        if let Some(ViewKind::File(v)) = app.views.get(&vid) {
            assert_eq!(v.line_count(), 2, "the view reloaded the edited file");
        } else {
            panic!("view gone");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Apply events until the requested view's `FileRead` has been handled, or
    /// the deadline passes.
    ///
    /// A scheduled read emits `FileRead` **and** `FileChanges`; the two arrive in
    /// whatever order the worker gets to them, and a previous read's
    /// `FileChanges` can still be in the queue. Waiting for the specific event
    /// makes the test independent of that timing (it was a CI-only flake: on a
    /// faster `git diff` the stale `FileChanges` was consumed instead of the
    /// re-read, so the view kept its old contents).
    fn pump_until_file_read(
        rx: &std::sync::mpsc::Receiver<AppEvent>,
        app: &mut App,
        expected: PaneId,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            let Ok(ev) = rx.recv_timeout(std::time::Duration::from_millis(250)) else {
                continue;
            };
            let was_read = matches!(&ev, AppEvent::FileRead { id, .. } if *id == expected);
            app.handle_event(ev);
            if was_read {
                return;
            }
        }
        panic!("no FileRead for {expected:?} arrived within the deadline");
    }

    /// Apply events until the requested directory's listing has been handled.
    /// A newly-created app also produces PTY and background-scan events, so
    /// consuming only the next event makes directory tests race those workers.
    fn pump_until_dir_read(
        rx: &std::sync::mpsc::Receiver<AppEvent>,
        app: &mut App,
        expected: &std::path::Path,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            let Ok(ev) = rx.recv_timeout(std::time::Duration::from_millis(250)) else {
                continue;
            };
            let was_read = matches!(&ev, AppEvent::DirRead { path, .. } if path == expected);
            app.handle_event(ev);
            if was_read {
                return;
            }
        }
        panic!(
            "no DirRead for {} arrived within the deadline",
            expected.display()
        );
    }

    /// Set a file's mtime, portable enough for the test (via a fresh write's
    /// natural mtime is unreliable at sub-second resolution, so bump explicitly).
    fn filetime_set(path: &std::path::Path, _when: std::time::SystemTime) {
        // Touch by rewriting; most filesystems give it a newer mtime than the
        // view's recorded one. If equal, sleep briefly and rewrite once more.
        let cur = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        let data = std::fs::read(path).unwrap();
        std::fs::write(path, &data).unwrap();
        if std::fs::metadata(path).and_then(|m| m.modified()).ok() == cur {
            std::thread::sleep(std::time::Duration::from_millis(1100));
            std::fs::write(path, &data).unwrap();
        }
    }

    /// Opening a file that is already open focuses the existing view instead of
    /// making a duplicate; `y` copies the whole file to the clipboard.
    #[test]
    fn reopening_focuses_existing_and_copy_yanks_content() {
        let _env = crate::persist::test_env("file-dedup-copy");
        let dir = std::env::temp_dir().join(format!("luvus-dc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, b"line one\nline two\n").unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();

        app.open_file_view(file.clone(), OpenTarget::Tab);
        let first = app.layout().focus;
        // Drain the read so content is present.
        pump_until_file_read(&rx, &mut app, first);
        let tabs_before = app.workspaces[app.active_ws].tabs.len();
        let views_before = app.views.len();

        // Re-open the same file: no new tab, no new view, and it is focused.
        app.open_file_view(file.clone(), OpenTarget::Tab);
        assert_eq!(
            app.workspaces[app.active_ws].tabs.len(),
            tabs_before,
            "no duplicate tab"
        );
        assert_eq!(app.views.len(), views_before, "no duplicate view");
        assert_eq!(app.layout().focus, first, "the existing view is focused");

        // `y` copies the whole file through the clipboard path.
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        app.handle_file_key(first, KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert_eq!(
            app.pending_clipboard.as_deref(),
            Some("line one\nline two"),
            "the file content is queued to the clipboard"
        );
        assert!(app.toast.is_some(), "a copy toast is shown");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Dragging the mouse across a file view selects text and copies it on
    /// release — the same drag-to-clipboard as a pane (docs/38).
    #[test]
    fn mouse_drag_selects_and_copies_file_text() {
        use crate::event::AppEvent;
        use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let _env = crate::persist::test_env("file-drag-copy");
        let dir = std::env::temp_dir().join(format!("luvus-md-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("s.txt");
        std::fs::write(&file, b"hello world\nsecond line\n").unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        app.open_file_view(file.clone(), OpenTarget::Tab);
        let vid = app.layout().focus;
        pump_until_file_read(&rx, &mut app, vid);

        // Render so `pane_content_rects` (needed for hit-testing the drag) is set.
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let content = app
            .pane_content_rects
            .iter()
            .find(|(id, _)| *id == vid)
            .map(|(_, r)| *r)
            .expect("the view has a content rect");

        // Drag across the first text line: text starts after the gutter.
        let gutter = crate::files::gutter_width(2);
        let x0 = content.x + gutter + 1; // first text column
        let y = content.y; // first visible line
        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x0,
            row: y,
            modifiers: KeyModifiers::NONE,
        };
        let drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: x0 + 4, // select "hello"
            row: y,
            modifiers: KeyModifiers::NONE,
        };
        let up = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: x0 + 4,
            row: y,
            modifiers: KeyModifiers::NONE,
        };
        app.handle_event(AppEvent::Mouse(down));
        app.handle_event(AppEvent::Mouse(drag));
        assert!(
            app.selection.is_some(),
            "a selection is built over the view"
        );
        app.handle_event(AppEvent::Mouse(up));

        assert_eq!(
            app.pending_clipboard.as_deref(),
            Some("hello"),
            "the dragged text was copied to the clipboard"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file view opened in a tab survives a save/restore round trip.
    #[test]
    fn file_tab_survives_restore() {
        let _env = crate::persist::test_env("file-tab-restore");
        let dir = std::env::temp_dir().join(format!("luvus-fvr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("keep.txt");
        std::fs::write(&file, b"persisted body\n").unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        app.open_file_view(file.clone(), OpenTarget::Tab);
        let snap = crate::persist::snapshot(&app);

        let (tx2, _rx2) = std::sync::mpsc::channel();
        let restored = App::from_snapshot(snap, tx2).expect("restore");
        // Exactly one file view came back, pointing at the same path.
        let paths: Vec<_> = restored
            .views
            .values()
            .map(|ViewKind::File(v)| v.path.clone())
            .collect();
        assert_eq!(paths, vec![file], "the file view was rebuilt on restore");

        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn file_view_frees_content_on_close() {
        let _env = crate::persist::test_env("file-mem-free");
        let dir = std::env::temp_dir().join(format!("luvus-mem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("big.txt");
        let body: String = (0..50_000).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&file, body).unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.open_file_view(file.clone(), OpenTarget::Tab);
        let vid = app.layout().focus;
        if let Some(ViewKind::File(v)) = app.views.get_mut(&vid) {
            v.apply(crate::files::read_file(&file));
            assert_eq!(v.line_count(), 50_000, "content held while open");
        }
        // Closing drops the view entirely — no lingering content.
        app.close_pane(vid);
        assert!(
            !app.views.contains_key(&vid),
            "view (and its 50k lines) freed on close"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn set_line_dock_no_stale_tail_when_row_shortens() {
        use ratatui::{backend::TestBackend, Terminal};
        let _env = crate::persist::test_env("stale-tail");
        let root = std::env::temp_dir().join(format!("luvus-st-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("VERYLONGFILENAME_abcdefghij.rs"), b"x").unwrap();
        std::fs::write(root.join("z_short.rs"), b"x").unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(60, 20, tx).unwrap();
        app.workspaces[app.active_ws].cwd = root.clone();
        app.sidebars.left.docks.push(crate::app::DockKind::Files);
        app.ensure_file_tree();
        app.file_tree
            .apply_dir(root.clone(), crate::files::read_dir_entries(&root));

        // The SAME Terminal reused across frames — this is where stale cells bite.
        let mut term = Terminal::new(TestBackend::new(60, 20)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        // Now hide the long file (show_hidden trick won't help; instead re-root to
        // an empty dir so the long row is replaced by nothing at that position).
        let empty = root.join("sub");
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::write(empty.join("z.rs"), b"x").unwrap();
        app.workspaces[app.active_ws].cwd = empty.clone();
        app.file_tree.set_root(empty.clone());
        app.file_tree
            .apply_dir(empty.clone(), crate::files::read_dir_entries(&empty));
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();

        let buf = term.backend().buffer();
        let full: String = (0..buf.area.height)
            .map(|r| {
                (0..buf.area.width)
                    .map(|c| buf.cell((c, r)).map(|x| x.symbol()).unwrap_or(" "))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !full.contains("VERYLONGFILENAME"),
            "stale tail from the previous longer row leaked:\n{full}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
    /// Clicking a folder schedules its read immediately (not on the next 1 Hz
    /// tick), so it loads without a visible lag.
    #[test]
    fn expanding_a_folder_loads_it_immediately() {
        let _env = crate::persist::test_env("file-expand-now");
        let root = std::env::temp_dir().join(format!("luvus-ex-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/inner.rs"), b"x").unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.workspaces[app.active_ws].cwd = root.clone();
        app.sidebars.left.docks.push(DockKind::Files);
        app.ensure_file_tree();
        // Apply the root read so `sub` is a visible row.
        pump_until_dir_read(&rx, &mut app, &root);

        // Click `sub` to expand it — WITHOUT calling ensure_file_tree again.
        let idx = app
            .file_tree
            .visible_rows()
            .iter()
            .position(|r| r.name == "sub")
            .expect("sub row");
        app.file_row_activate(idx, OpenTarget::Tab);

        // A read for `sub` must already be in flight — arrives without any tick.
        pump_until_dir_read(&rx, &mut app, &root.join("sub"));
        assert!(
            app.file_tree
                .visible_rows()
                .iter()
                .any(|r| r.name == "inner.rs"),
            "the folder's contents loaded right after the click"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
    /// Closing a file tab via the ✕ / prefix-X path (`close_tab`, not `close_pane`)
    /// must forget its view, so the same file can be opened again. Regression for
    /// the reported bug: after open → close → the row became un-clickable because a
    /// stale `views` entry made `open_file_view` focus a tab that no longer existed.
    #[test]
    fn closing_a_file_tab_lets_it_reopen() {
        let _env = crate::persist::test_env("file-reopen");
        let dir = std::env::temp_dir().join(format!("luvus-reopen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("r.txt");
        std::fs::write(&file, b"body\n").unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();

        // Open in a tab (the plain-click default).
        app.open_file_view(file.clone(), OpenTarget::Tab);
        let vid = app.layout().focus;
        assert!(app.views.contains_key(&vid), "the file view is open");
        let file_tab = app.workspaces[app.active_ws].active_tab;

        // Close it the way the ✕ / prefix-X does — NOT through close_pane.
        app.close_tab(file_tab);
        assert!(
            !app.views.contains_key(&vid),
            "the view is forgotten when its tab is closed (no orphan)"
        );

        // Reopen the same file: a brand-new view leaf is created and focused,
        // rather than silently focusing the dead one (which read as un-clickable).
        app.open_file_view(file.clone(), OpenTarget::Tab);
        let vid2 = app.layout().focus;
        assert!(app.views.contains_key(&vid2), "reopened into a fresh view");
        assert_ne!(vid, vid2, "a new leaf, not the stale id");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// End-to-end through the real mouse path: click a file row to open it, close
    /// its tab, then click the row again — it must reopen. Reproduces the reported
    /// "won't open the second time" via `handle_event(Mouse)`, not direct calls.
    #[test]
    fn clicking_a_file_reopens_after_its_tab_is_closed() {
        use crate::event::AppEvent;
        use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let _env = crate::persist::test_env("file-click-reopen");
        let root = std::env::temp_dir().join(format!("luvus-cr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("hello.txt"), b"hi\n").unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        app.workspaces[app.active_ws].cwd = root.clone();
        app.sidebars.left.docks.push(DockKind::Files);
        app.ensure_file_tree();
        while let Ok(ev) = rx.recv_timeout(std::time::Duration::from_millis(300)) {
            app.handle_event(ev);
        }

        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        let click_file = |app: &mut App, term: &mut Terminal<TestBackend>| {
            term.draw(|f| crate::ui::render(f, app)).unwrap();
            let (_, rect) = app
                .file_tree_rects
                .iter()
                .find(|(i, _)| app.file_tree.visible_rows()[*i].name == "hello.txt")
                .cloned()
                .expect("hello.txt has a clickable rect");
            let down = MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: rect.x + 3,
                row: rect.y,
                modifiers: KeyModifiers::NONE,
            };
            app.handle_event(AppEvent::Mouse(down));
        };

        // First click: opens a file view.
        click_file(&mut app, &mut term);
        assert_eq!(app.views.len(), 1, "first click opened the file");
        let first = app.layout().focus;

        // Close its tab the way the tab ✕ does.
        let file_tab = app.workspaces[app.active_ws].active_tab;
        app.close_tab(file_tab);
        assert_eq!(app.views.len(), 0, "closing the tab forgot the view");

        // Second click on the same row: must reopen (a fresh view leaf).
        click_file(&mut app, &mut term);
        assert_eq!(
            app.views.len(),
            1,
            "clicking the file again reopened it (the reported bug)"
        );
        assert_ne!(app.layout().focus, first, "reopened into a new leaf");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A line longer than the pane wraps onto the next row instead of being
    /// clipped at the right edge, so no content is hidden (the reported bug).
    #[test]
    fn long_line_wraps_and_shows_its_tail() {
        let _env = crate::persist::test_env("file-wrap");
        let dir = std::env::temp_dir().join(format!("luvus-wrap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("long.md");
        // One line far wider than a narrow pane; unique head and tail words.
        let body = "HEADWORD ".to_string() + &"filler ".repeat(20) + "TAILWORD";
        std::fs::write(&file, &body).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App::new(40, 20, tx).unwrap();
        app.open_file_view(file.clone(), OpenTarget::Tab);
        let vid = app.layout().focus;
        pump_until_file_read(&rx, &mut app, vid);
        assert!(
            matches!(
                app.views.get(&vid),
                Some(ViewKind::File(v)) if v.wrap
            ),
            "the view opened wrapped"
        );

        let mut term = Terminal::new(TestBackend::new(40, 20)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let text = buffer_text(&term);
        assert!(text.contains("HEADWORD"), "the head of the line renders");
        assert!(
            text.contains("TAILWORD"),
            "the tail wraps onto a later row and is visible, not clipped:\n{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file created outside luvus (agent, terminal, another process) appears in
    /// the tree on the next rescan, and an unchanged rescan does not churn it.
    #[test]
    fn external_new_file_is_picked_up_by_rescan() {
        let _env = crate::persist::test_env("file-external-add");
        let root = std::env::temp_dir().join(format!("luvus-ext-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("existing.rs"), b"x").unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 30, tx).unwrap();
        app.workspaces[app.active_ws].cwd = root.clone();
        app.sidebars.left.docks.push(DockKind::Files);
        app.ensure_file_tree();
        while let Ok(ev) = rx.recv_timeout(std::time::Duration::from_millis(300)) {
            app.handle_event(ev);
        }
        assert!(
            app.file_tree
                .visible_rows()
                .iter()
                .any(|r| r.name == "existing.rs"),
            "the initial file is in the tree"
        );
        // Nothing changed on disk: a rescan (forced by resetting the gate) must not
        // mark the tree dirty, so the cached rows are reused.
        app.last_file_scan_at = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(10))
            .unwrap();
        app.ensure_file_tree();
        while let Ok(ev) = rx.recv_timeout(std::time::Duration::from_millis(300)) {
            app.handle_event(ev);
        }

        // A new file appears WITHOUT going through luvus's own CRUD.
        std::fs::write(root.join("dropped.rs"), b"y").unwrap();
        // Force the rescan gate open and tick again.
        app.last_file_scan_at = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(10))
            .unwrap();
        app.ensure_file_tree();
        while let Ok(ev) = rx.recv_timeout(std::time::Duration::from_millis(300)) {
            app.handle_event(ev);
        }
        assert!(
            app.file_tree
                .visible_rows()
                .iter()
                .any(|r| r.name == "dropped.rs"),
            "the externally-created file showed up after a rescan"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The right-click menu creates, renames, and deletes on disk (docs/38 FILE-6).
    #[test]
    fn file_menu_crud_creates_renames_deletes() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let _env = crate::persist::test_env("file-crud");
        let root = std::env::temp_dir().join(format!("luvus-crud-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/old.rs"), b"x").unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 30, tx).unwrap();
        app.workspaces[app.active_ws].cwd = root.clone();
        app.sidebars.left.docks.push(DockKind::Files);
        app.ensure_file_tree();
        // drain root read + expand src
        while let Ok(ev) = rx.recv_timeout(std::time::Duration::from_millis(300)) {
            app.handle_event(ev);
        }
        let src_idx = app
            .file_tree
            .visible_rows()
            .iter()
            .position(|r| r.name == "src")
            .unwrap();
        app.file_row_activate(src_idx, OpenTarget::Tab); // expand
        while let Ok(ev) = rx.recv_timeout(std::time::Duration::from_millis(300)) {
            app.handle_event(ev);
        }

        let typ = |app: &mut App, s: &str| {
            for c in s.chars() {
                app.file_prompt_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
            }
        };

        // New file inside `src`.
        let src_idx = app
            .file_tree
            .visible_rows()
            .iter()
            .position(|r| r.name == "src")
            .unwrap();
        app.open_file_menu(src_idx, 5, 5);
        app.file_menu_action_pub(crate::app::FileMenuItem::NewFile);
        typ(&mut app, "created.rs");
        app.file_prompt_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            root.join("src/created.rs").exists(),
            "new file created on disk"
        );

        // Rename old.rs -> new.rs.
        while let Ok(ev) = rx.recv_timeout(std::time::Duration::from_millis(300)) {
            app.handle_event(ev);
        }
        let old_idx = app
            .file_tree
            .visible_rows()
            .iter()
            .position(|r| r.name == "old.rs")
            .unwrap();
        app.open_file_menu(old_idx, 5, 6);
        app.file_menu_action_pub(crate::app::FileMenuItem::Rename);
        // clear the pre-filled name then type the new one
        for _ in 0..20 {
            app.file_prompt_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        }
        typ(&mut app, "new.rs");
        app.file_prompt_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            root.join("src/new.rs").exists() && !root.join("src/old.rs").exists(),
            "renamed"
        );

        // Delete created.rs.
        while let Ok(ev) = rx.recv_timeout(std::time::Duration::from_millis(300)) {
            app.handle_event(ev);
        }
        let c_idx = app
            .file_tree
            .visible_rows()
            .iter()
            .position(|r| r.name == "created.rs")
            .unwrap();
        app.open_file_menu(c_idx, 5, 7);
        app.file_menu_action_pub(crate::app::FileMenuItem::Delete);
        app.file_delete_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(!root.join("src/created.rs").exists(), "deleted");

        let _ = std::fs::remove_dir_all(&root);
    }
    /// Delete requires the confirm modal: choosing Delete does NOT remove the
    /// file, cancelling leaves it, and only `y`/⏎ actually deletes.
    #[test]
    fn delete_needs_confirmation() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let _env = crate::persist::test_env("file-del-guard");
        let root = std::env::temp_dir().join(format!("luvus-dg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("keep.rs");
        std::fs::write(&file, b"x").unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.workspaces[app.active_ws].cwd = root.clone();
        app.sidebars.left.docks.push(DockKind::Files);
        app.ensure_file_tree();
        while let Ok(ev) = rx.recv_timeout(std::time::Duration::from_millis(300)) {
            app.handle_event(ev);
        }
        let idx = app
            .file_tree
            .visible_rows()
            .iter()
            .position(|r| r.name == "keep.rs")
            .unwrap();

        // Choosing Delete arms the confirm modal but does NOT touch disk.
        app.open_file_menu(idx, 5, 5);
        app.file_menu_action_pub(crate::app::FileMenuItem::Delete);
        assert!(app.file_delete.is_some(), "the confirm modal is armed");
        assert!(
            file.exists(),
            "nothing deleted yet — waiting on confirmation"
        );

        // Cancelling (Esc) leaves the file and closes the modal.
        app.file_delete_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            app.file_delete.is_none() && file.exists(),
            "cancel keeps the file"
        );

        // Only y/Enter deletes.
        app.open_file_menu(idx, 5, 5);
        app.file_menu_action_pub(crate::app::FileMenuItem::Delete);
        app.file_delete_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(!file.exists(), "confirmed delete removes it");

        let _ = std::fs::remove_dir_all(&root);
    }
}
