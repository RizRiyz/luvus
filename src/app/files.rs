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

const RECENT_FILE_CAP: usize = 12;

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

/// What an `Insert Path` did.
///
/// The outcome is returned rather than only toasted so a caller — and the tests
/// for it — can see the exact text that reached the pane and which pane took
/// it, without reaching into the PTY.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InsertPath {
    /// The path was handed to `target`'s paste path, as exactly this text.
    Inserted { target: PaneId, text: String },
    /// Nothing to insert into: the focused leaf is a native view, or the tab
    /// has no pane at all.
    NoPane,
    /// The path holds a terminal control character and cannot be pasted safely.
    ControlCharacter,
}

impl App {
    /// Keep the FILES dock honest, off the render path. Called from `detect_tick`:
    /// re-roots the tree to the active node, then schedules a worker read for any
    /// directory that should be on screen but has not been read yet. Cheap when
    /// there is nothing to do (a few `HashSet` checks), and a no-op when the dock
    /// isn't mounted.
    pub fn ensure_file_tree(&mut self) {
        let dock_visible = self.sidebars.side_of(&DockKind::Files).is_some();
        let diff_visible = self
            .layout()
            .leaves()
            .into_iter()
            .any(|id| matches!(self.views.get(&id), Some(ViewKind::Diff(_))));
        if !dock_visible && !diff_visible {
            return;
        }
        let cwd = self.ws().cwd.clone();
        self.file_tree.set_root(cwd);
        let dirty_views: Vec<_> = self
            .layout()
            .leaves()
            .into_iter()
            .filter(|id| matches!(self.views.get(id), Some(ViewKind::Diff(view)) if view.dirty))
            .collect();
        for id in dirty_views {
            self.schedule_diff_read(id);
        }
        if dock_visible {
            self.load_pending_dirs();
            self.rescan_file_tree();
        }
        self.refresh_git_status();
    }

    /// Park a first `files.tree` call until its root listing has returned from
    /// the filesystem worker. Cached trees answer inline; no directory I/O is
    /// ever moved onto the app loop just to make the CLI deterministic.
    pub(crate) fn prepare_files_api(
        &mut self,
        req: crate::ipc::api::ApiRequest,
    ) -> Option<crate::ipc::api::ApiRequest> {
        if req.method != "files.tree" {
            return Some(req);
        }
        self.prepare_file_tree_api(false);
        if self.file_tree.root_loaded() {
            return Some(req);
        }
        self.pending_file_tree_api
            .push((self.ws().cwd.clone(), req));
        None
    }

    pub(crate) fn finish_pending_files_api(&mut self, completed: &Path) {
        let active_root = self.ws().cwd.clone();
        let root_loaded = self.file_tree.root_loaded();
        let mut pending = std::mem::take(&mut self.pending_file_tree_api);
        for (root, req) in pending.drain(..) {
            if !crate::platform::same_path(&root, &active_root) {
                let _ = req.reply.send(
                    serde_json::json!({"id":req.id,"error":{
                        "code":"files_error",
                        "message":"active workspace changed while FILES was loading"
                    }})
                    .to_string(),
                );
            } else if root_loaded && crate::platform::same_path(completed, &root) {
                let response = self.handle_api(&req);
                let _ = req.reply.send(response);
            } else {
                self.pending_file_tree_api.push((root, req));
            }
        }
    }

    /// Fail parked FILES requests for one workspace while preserving requests
    /// whose directory workers still belong to another open workspace.
    pub(crate) fn fail_pending_files_api_for_root(&mut self, closed_root: &Path, message: &str) {
        let mut pending = std::mem::take(&mut self.pending_file_tree_api);
        for (root, req) in pending.drain(..) {
            if crate::platform::same_path(&root, closed_root) {
                let _ = req.reply.send(
                    serde_json::json!({"id":req.id,"error":{
                        "code":"files_error",
                        "message":message
                    }})
                    .to_string(),
                );
            } else {
                self.pending_file_tree_api.push((root, req));
            }
        }
    }

    /// Fail every parked FILES request when no workspace remains.
    pub(crate) fn fail_pending_files_api(&mut self, message: &str) {
        for (_, req) in self.pending_file_tree_api.drain(..) {
            let _ = req.reply.send(
                serde_json::json!({"id":req.id,"error":{
                    "code":"files_error",
                    "message":message
                }})
                .to_string(),
            );
        }
    }

    /// Root and schedule the FILES tree for an explicit API request even when
    /// the dock is hidden. Periodic upkeep deliberately sleeps while no FILES or
    /// DIFF surface is visible, but `files.tree` and `files.refresh` must not
    /// depend on a client attaching after server restore.
    pub(crate) fn prepare_file_tree_api(&mut self, invalidate: bool) {
        let root = self.ws().cwd.clone();
        self.file_tree.set_root(root);
        if invalidate {
            self.file_tree.invalidate();
        }
        self.load_pending_dirs();
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

    /// Refresh the shared FILES tint and DIFF index off the loop (docs/88).
    /// One structured status scan preserves the existing two-second cadence.
    fn refresh_git_status(&mut self) {
        self.refresh_diff_status(false);
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
        for (id, view) in self.views.iter() {
            if let ViewKind::File(v) = view {
                let disk = std::fs::metadata(&v.path).and_then(|m| m.modified()).ok();
                if disk.is_some() && disk != v.mtime {
                    stale.push((*id, v.path.clone(), disk));
                }
            }
        }
        for (id, path, mtime) in stale {
            if let Some(ViewKind::File(v)) = self.views.get_mut(&id) {
                v.mtime = mtime; // record now so we don't reschedule until it changes again
            }
            self.schedule_file_read(id, path);
        }
    }

    /// `Ctrl+Space e`: unmount the FILES dock, or restore it to the side where
    /// the user last placed it. Mounting also makes sure that side is visible.
    pub fn toggle_files_dock(&mut self) {
        if self.sidebars.side_of(&DockKind::Files).is_some() {
            self.unmount_dock(&DockKind::Files);
        } else {
            let target = self.sidebars.files_side;
            if self.sidebars.has_room(target) {
                self.sidebars.get_mut(target).visible = true;
            }
            self.move_dock(&DockKind::Files, target);
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

    /// Open a fuzzy-finder file result in a whole tab in its owning workspace.
    /// Reuse an existing whole-file tab, but never redirect the user into a
    /// preview or split pane that happens to show the same path.
    pub fn open_file_search_result(&mut self, path: PathBuf) {
        match self.file_open_editor() {
            Some(cmd) => self.open_file_in_editor(path, &cmd),
            None => {
                self.remember_file(&path);
                if let Some(id) = self.file_tab_showing(&path) {
                    self.focus_pane_global(id);
                } else {
                    self.create_file_view(path, OpenTarget::Tab);
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
    ///
    /// Re-opening a file in the editor it is *already* open in focuses that tab
    /// instead of launching a second one (docs/38): the read-only viewer has
    /// always de-duplicated this way, and a second `vim` on one file would only
    /// fight the first over its swap file. A **different** editor is not a
    /// duplicate — `Open With` is an explicit per-file override — so it opens its
    /// own tab.
    pub fn open_file_in_editor(&mut self, path: PathBuf, editor: &str) {
        if let Some(id) = self.editor_tab_showing(&path, editor) {
            self.remember_file(&path);
            self.focus_pane_global(id);
            return;
        }
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
                self.editor_files.insert(
                    id,
                    crate::app::EditorFile {
                        path: path.clone(),
                        command: editor.to_string(),
                    },
                );
                self.remember_file(&path);
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
            FileMenuItem::InsertPath => {
                match self.insert_path(&menu.path) {
                    InsertPath::Inserted { .. } => {}
                    // Both refusals are silent-looking otherwise: the menu closes
                    // and nothing appears in the prompt.
                    InsertPath::NoPane => self.show_toast("no pane to insert into"),
                    InsertPath::ControlCharacter => {
                        self.show_toast("path contains a control character")
                    }
                }
            }
            FileMenuItem::Delete => self.file_delete = Some(menu.path),
            FileMenuItem::Divider => {}
        }
    }

    /// Type `path` into the focused pane's prompt, leaving it unsubmitted.
    ///
    /// The path is absolute, exactly as `Copy Path` gives it, so it stays right
    /// even when the pane has since changed its working directory. It arrives
    /// through [`App::paste_into_focused_pane`] rather than a write at the pane,
    /// which is what keeps bracketed paste, scroll position and activity
    /// tracking the same as any other paste — and no `\r` follows it, because
    /// composing the rest of the command line is the user's job.
    ///
    /// The target is whatever the active tab already had focused: opening the
    /// FILES dock and its context menu never moves pane focus, so the pane the
    /// user was typing in is still the one that receives this.
    ///
    /// A path holding a control character is refused rather than trimmed.
    /// Besides `\n` and `\r` submitting a prompt, Escape and other controls can
    /// alter terminal input or terminate bracketed-paste mode. These characters
    /// are legal in Unix filenames, but no truncation of the path is a better
    /// answer than inserting unsafe or incorrect text.
    pub(crate) fn insert_path(&mut self, path: &Path) -> InsertPath {
        let text = path.to_string_lossy().into_owned();
        if text.chars().any(char::is_control) {
            return InsertPath::ControlCharacter;
        }
        match self.paste_into_focused_pane(&text) {
            Some(target) => InsertPath::Inserted { target, text },
            None => InsertPath::NoPane,
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
        self.ws()
            .tabs
            .iter()
            .flat_map(|tab| tab.layout.leaves())
            .find(
                |id| matches!(self.views.get(id), Some(ViewKind::File(view)) if view.path == path),
            )
    }

    /// A native file view that owns its whole tab, excluding preview/split panes.
    fn file_tab_showing(&self, path: &std::path::Path) -> Option<PaneId> {
        self.ws().tabs.iter().find_map(|tab| {
            let leaves = tab.layout.leaves();
            let [id] = leaves.as_slice() else {
                return None;
            };
            matches!(self.views.get(id), Some(ViewKind::File(view)) if view.path == path)
                .then_some(*id)
        })
    }

    /// A tab whose whole content is `editor` already running on `path` — the
    /// terminal-editor twin of `file_tab_showing`. An editor tab has no `views`
    /// entry to match against (it is an ordinary PTY pane), so `editor_files` is
    /// the only record of what a tab is editing. That map is cleared by
    /// `drop_leaf_runtime`, so a quit editor stops matching here and the next
    /// click gets a fresh one rather than focus into a dead pane.
    ///
    /// The command comparison is exact, not normalized: every caller passes a
    /// run-command taken straight out of `self.editors` (the default resolved by
    /// `file_open_editor`, or the one `Open With` snapshotted from the same
    /// list), so both sides are the same canonical string.
    fn editor_tab_showing(&self, path: &std::path::Path, editor: &str) -> Option<PaneId> {
        self.ws().tabs.iter().find_map(|tab| {
            let leaves = tab.layout.leaves();
            let [id] = leaves.as_slice() else {
                return None;
            };
            self.editor_files
                .get(id)
                .is_some_and(|open| open.path == path && open.command == editor)
                .then_some(*id)
        })
    }

    /// Open `path` in a native file view (docs/38 FILE-3). `Preview` reuses the
    /// one preview pane in the active workspace; `Pane` splits a fresh permanent
    /// pane; `Tab` opens a new tab. The file is read on a worker thread and
    /// applied via `FileRead`.
    pub fn open_file_view(&mut self, path: PathBuf, target: OpenTarget) {
        self.remember_file(&path);
        // Already open? Focus that view instead of opening a duplicate.
        if let Some(id) = self.view_showing(&path) {
            self.focus_pane_global(id);
            return;
        }
        // Reuse the live preview pane: just swap its content.
        if target == OpenTarget::Preview {
            if let Some(id) = self.active_preview_view() {
                self.set_view_file(id, path);
                self.focus_pane_global(id);
                return;
            }
        }

        self.create_file_view(path, target);
    }

    fn create_file_view(&mut self, path: PathBuf, target: OpenTarget) {
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
            self.preview_views.insert(id);
        }
        self.schedule_file_read(id, path);
        self.mode = Mode::Normal;
    }

    /// Point an existing view leaf at a different file and re-read it.
    fn set_view_file(&mut self, id: PaneId, path: PathBuf) {
        self.remember_file(&path);
        if let Some(ViewKind::File(v)) = self.views.get_mut(&id) {
            *v = FileView::new(path.clone());
        }
        self.schedule_file_read(id, path);
    }

    fn remember_file(&mut self, path: &Path) {
        let workspace = self.ws().cwd.clone();
        self.recent_files
            .retain(|(cwd, existing)| cwd != &workspace || existing != path);
        self.recent_files
            .push_front((workspace, path.to_path_buf()));
        self.recent_files.truncate(RECENT_FILE_CAP);
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
            Some(ViewKind::Diff(_)) => None,
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
    use crate::app::{DockKind, FileMenu, FileMenuItem, Side};
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn files_toggle_restores_last_side_across_restart() {
        let _env = crate::persist::test_env("files-toggle-side");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();

        assert!(app.move_dock(&DockKind::Files, Side::Right));
        assert_eq!(app.sidebars.side_of(&DockKind::Files), Some(Side::Right));
        app.toggle_files_dock();
        assert_eq!(app.sidebars.side_of(&DockKind::Files), None);
        assert_eq!(
            app.config
                .sidebars
                .as_ref()
                .and_then(|sidebars| sidebars.files_side),
            Some(Side::Right),
            "hiding FILES keeps its last placement in config"
        );

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut reopened = App::new(80, 24, tx).unwrap();
        assert_eq!(reopened.sidebars.side_of(&DockKind::Files), None);
        reopened.toggle_files_dock();
        assert_eq!(
            reopened.sidebars.side_of(&DockKind::Files),
            Some(Side::Right),
            "showing FILES restores the persisted side"
        );
        assert!(
            reopened.sidebars.right.visible,
            "showing FILES also reveals its sidebar"
        );
    }

    #[test]
    fn recent_files_are_deduplicated_newest_first_and_bounded() {
        let _env = crate::persist::test_env("file-recent-bounded");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        for index in 0..RECENT_FILE_CAP + 3 {
            app.remember_file(Path::new(&format!("file-{index}.rs")));
        }
        assert_eq!(app.recent_files.len(), RECENT_FILE_CAP);
        assert!(app.recent_files.front().unwrap().1.ends_with("file-14.rs"));
        assert!(app.recent_files.back().unwrap().1.ends_with("file-3.rs"));

        app.remember_file(Path::new("file-8.rs"));
        assert_eq!(app.recent_files.len(), RECENT_FILE_CAP);
        assert!(app.recent_files.front().unwrap().1.ends_with("file-8.rs"));
        assert_eq!(
            app.recent_files
                .iter()
                .filter(|(_, path)| path.ends_with("file-8.rs"))
                .count(),
            1
        );
    }

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

        // `cat` stands in for an editor: a real program launched with the file as
        // a literal argv element (present on every unix CI runner).
        app.open_file_in_editor(file.clone(), "cat");

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
            app.editor_files.get(&focus).map(|open| open.path.as_path()),
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

    /// A second click on an already-open file must land in the tab that is
    /// already showing it, not stack up another one. Read-only half: the click
    /// path (`open_file_at`) goes through `open_file_view`'s `view_showing` guard.
    #[test]
    fn clicking_a_file_twice_reuses_its_readonly_tab() {
        let _env = crate::persist::test_env("file-click-readonly-dedup");
        let dir = std::env::temp_dir().join(format!("luvus-cr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("once.txt");
        std::fs::write(&file, b"hello\n").unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        assert_eq!(app.file_open_editor(), None, "read-only is the default");
        let tabs_before = app.workspaces[app.active_ws].tabs.len();

        app.open_file_at(file.clone(), None);
        let first = app.layout().focus;
        assert_eq!(
            app.workspaces[app.active_ws].tabs.len(),
            tabs_before + 1,
            "the first click opens a tab"
        );

        // Move away first, so "focused" is a real effect of the second click and
        // not just where we already were.
        app.workspaces[app.active_ws].active_tab = 0;
        app.open_file_at(file.clone(), None);
        assert_eq!(
            app.workspaces[app.active_ws].tabs.len(),
            tabs_before + 1,
            "the second click adds no tab"
        );
        assert_eq!(app.layout().focus, first, "it focuses the open view");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The editor half of the same rule: with `Open files with:` set, a second
    /// click focuses the running editor instead of spawning a rival one on the
    /// same file. Nothing but `editor_files` records what an editor tab holds.
    #[test]
    fn clicking_a_file_twice_reuses_its_editor_tab() {
        let _env = crate::persist::test_env("file-click-editor-dedup");
        let dir = std::env::temp_dir().join(format!("luvus-ce-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("edit.txt");
        std::fs::write(&file, b"hello\n").unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        // `cat` stands in for the editor, as elsewhere in these tests: a real
        // program that takes the file as a literal argv element.
        app.editors = vec![("cat".to_string(), "cat".to_string())];
        app.config.layout.file_open = "cat".to_string();
        let tabs_before = app.workspaces[app.active_ws].tabs.len();

        app.open_file_at(file.clone(), None);
        let first = app.layout().focus;
        assert_eq!(
            app.workspaces[app.active_ws].tabs.len(),
            tabs_before + 1,
            "the first click opens an editor tab"
        );
        assert!(app.panes.contains_key(&first), "the editor is a PTY pane");

        app.workspaces[app.active_ws].active_tab = 0;
        app.open_file_at(file.clone(), None);
        assert_eq!(
            app.workspaces[app.active_ws].tabs.len(),
            tabs_before + 1,
            "the second click adds no tab"
        );
        assert_eq!(app.layout().focus, first, "it focuses the running editor");
        assert_eq!(app.panes.len(), 2, "no second editor process was spawned");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// De-duplication is per path, not "one editor tab at a time": a different
    /// file still gets its own tab.
    #[test]
    fn different_files_get_their_own_editor_tabs() {
        let _env = crate::persist::test_env("file-editor-two-files");
        let dir = std::env::temp_dir().join(format!("luvus-c2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let one = dir.join("one.txt");
        let two = dir.join("two.txt");
        std::fs::write(&one, b"1\n").unwrap();
        std::fs::write(&two, b"2\n").unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        app.editors = vec![("cat".to_string(), "cat".to_string())];
        app.config.layout.file_open = "cat".to_string();
        let tabs_before = app.workspaces[app.active_ws].tabs.len();

        app.open_file_at(one.clone(), None);
        let first = app.layout().focus;
        app.open_file_at(two.clone(), None);
        let second = app.layout().focus;

        assert_ne!(first, second, "the second file got its own pane");
        assert_eq!(
            app.workspaces[app.active_ws].tabs.len(),
            tabs_before + 2,
            "two files, two editor tabs"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `Open With` is an explicit per-file override, not a duplicate: a file
    /// already open in one editor must still launch in a *different* one picked
    /// from the right-click menu. De-duplicating on the path alone swallowed it.
    #[test]
    fn open_with_a_different_editor_still_launches_it() {
        let _env = crate::persist::test_env("file-open-with-other-editor");
        let dir = std::env::temp_dir().join(format!("luvus-cw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("both.txt");
        std::fs::write(&file, b"hello\n").unwrap();

        // `cat` and `head` stand in for two installed editors: real programs that
        // take the file as a literal argv element, as elsewhere in these tests.
        let editors = vec![
            ("cat".to_string(), "cat".to_string()),
            ("head".to_string(), "head".to_string()),
        ];
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        app.editors = editors.clone();
        app.config.layout.file_open = "cat".to_string();
        let tabs_before = app.workspaces[app.active_ws].tabs.len();
        let menu = |editors: &[(String, String)]| FileMenu {
            path: file.clone(),
            is_dir: false,
            anchor: (0, 0),
            items: Vec::new(),
            editors: editors.to_vec(),
        };

        // A plain click opens the configured default, `cat`.
        app.open_file_at(file.clone(), None);
        let first = app.layout().focus;

        // Right-click the same file and pick the *other* editor.
        app.file_menu = Some(menu(&editors));
        app.file_menu_action_pub(FileMenuItem::OpenWith(1));
        let second = app.layout().focus;
        assert_ne!(second, first, "the override got a pane of its own");
        assert_eq!(
            app.workspaces[app.active_ws].tabs.len(),
            tabs_before + 2,
            "two editors on one file means two tabs"
        );
        assert_eq!(
            app.editor_files
                .get(&second)
                .map(|open| open.command.as_str()),
            Some("head"),
            "the new tab runs the editor the user actually picked"
        );

        // The same editor is still a duplicate: picking `cat` again focuses it.
        app.file_menu = Some(menu(&editors));
        app.file_menu_action_pub(FileMenuItem::OpenWith(0));
        assert_eq!(
            app.layout().focus,
            first,
            "re-picking the running editor focuses its tab"
        );
        assert_eq!(
            app.workspaces[app.active_ws].tabs.len(),
            tabs_before + 2,
            "and opens no third tab"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Quitting the editor must leave nothing behind to focus: `PtyExit` closes
    /// the pane, which untracks its file, so the next click launches a new
    /// editor rather than jumping to a tab that no longer exists.
    #[test]
    fn a_closed_editor_tab_reopens_on_the_next_click() {
        use crate::event::AppEvent;
        let _env = crate::persist::test_env("file-editor-reopen");
        let dir = std::env::temp_dir().join(format!("luvus-cq-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("quit.txt");
        std::fs::write(&file, b"hello\n").unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        app.editors = vec![("cat".to_string(), "cat".to_string())];
        app.config.layout.file_open = "cat".to_string();
        let tabs_before = app.workspaces[app.active_ws].tabs.len();

        app.open_file_at(file.clone(), None);
        let first = app.layout().focus;

        // The editor quits.
        app.handle_event(AppEvent::PtyExit(first));
        assert_eq!(
            app.workspaces[app.active_ws].tabs.len(),
            tabs_before,
            "the editor tab closed with its process"
        );
        assert!(
            !app.editor_files.contains_key(&first),
            "the dead pane no longer claims the file"
        );

        app.open_file_at(file.clone(), None);
        let second = app.layout().focus;
        assert_ne!(second, first, "a fresh editor pane, not the dead id");
        assert!(app.panes.contains_key(&second), "the new editor is running");
        assert_eq!(
            app.workspaces[app.active_ws].tabs.len(),
            tabs_before + 1,
            "the file opened again in a tab of its own"
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
        let header_y = app.files_mode_rects[0].1.y;
        let first_row_y = app
            .file_tree_rects
            .iter()
            .map(|(_, rect)| rect.y)
            .min()
            .expect("the file list has rows");
        assert_eq!(
            first_row_y,
            header_y + 1,
            "the list starts directly below FILES/DIFF without an identity row"
        );

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
            app.views.get(&vid).and_then(|view| match view {
                ViewKind::File(view) => Some(view.line_count()),
                ViewKind::Diff(_) => None,
            }),
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
            .filter_map(|view| match view {
                ViewKind::File(view) => Some(view.path.clone()),
                ViewKind::Diff(_) => None,
            })
            .collect();
        assert_eq!(paths, vec![file], "the file view was rebuilt on restore");

        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn hidden_files_api_re_roots_and_reloads_after_restore() {
        let _env = crate::persist::test_env("files-api-restore");
        let root = std::env::temp_dir().join(format!("luvus-far-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("restored.txt"), b"restored\n").unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(100, 30, tx).unwrap();
        app.workspaces[app.active_ws].cwd = root.clone();
        app.sidebars
            .left
            .docks
            .retain(|dock| dock != &DockKind::Files);
        app.sidebars
            .right
            .docks
            .retain(|dock| dock != &DockKind::Files);
        let snapshot = crate::persist::snapshot(&app);

        let (tx, rx) = std::sync::mpsc::channel();
        let mut restored = App::from_snapshot(snapshot, tx).expect("restore");
        assert!(
            restored.sidebars.side_of(&DockKind::Files).is_none(),
            "the regression requires a hidden FILES dock"
        );
        assert!(restored.file_tree.root().as_os_str().is_empty());

        let request_tree = |id: &str, app: &mut App| -> std::sync::mpsc::Receiver<String> {
            let (reply, response) = std::sync::mpsc::channel();
            app.handle_event(AppEvent::Api(crate::ipc::api::ApiRequest {
                id: id.to_string(),
                method: "files.tree".to_string(),
                params: serde_json::json!({}),
                reply,
            }));
            response
        };

        let first = request_tree("first", &mut restored);
        assert!(
            first
                .recv_timeout(std::time::Duration::from_millis(10))
                .is_err(),
            "the first tree waits for its off-loop root read"
        );
        pump_until_dir_read(&rx, &mut restored, &root);
        let loaded: serde_json::Value = serde_json::from_str(
            &first
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("first populated tree"),
        )
        .unwrap();
        assert_eq!(loaded["result"]["root"], root.to_string_lossy().as_ref());
        assert!(loaded["result"]["rows"]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|row| row["name"] == "restored.txt")));

        restored
            .dispatch("files.refresh", &serde_json::json!({}))
            .unwrap();
        let refreshed = request_tree("refreshed", &mut restored);
        pump_until_dir_read(&rx, &mut restored, &root);
        let refreshed: serde_json::Value = serde_json::from_str(
            &refreshed
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("refreshed populated tree"),
        )
        .unwrap();
        assert_eq!(refreshed["result"]["root"], root.to_string_lossy().as_ref());
        assert!(refreshed["result"]["rows"]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|row| row["name"] == "restored.txt")));

        let _ = std::fs::remove_dir_all(&root);
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

    // ── Insert Path (docs/38 FILE-6) ─────────────────────────────────────────

    /// Build a FILES menu for `path` without going through the tree, the way
    /// the other menu tests here do.
    fn insert_menu(path: &Path) -> FileMenu {
        FileMenu {
            path: path.to_path_buf(),
            is_dir: false,
            anchor: (0, 0),
            items: Vec::new(),
            editors: Vec::new(),
        }
    }

    /// `Insert Path` types into the pane the user was already in, and inserts
    /// the absolute path *verbatim* — spaces intact, and with nothing appended.
    ///
    /// The missing trailing `\r` is the point: the path lands in the prompt for
    /// the user to finish the command around. Anything that submitted it would
    /// run a half-written command line.
    #[test]
    fn insert_path_types_the_path_into_the_focused_pane_without_submitting_it() {
        let _env = crate::persist::test_env("files-insert-path");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();

        // Two panes, so "the focused one" is a real choice rather than the only
        // pane there is.
        app.split(Axis::Col);
        assert_eq!(app.layout().len(), 2, "two panes to choose between");
        let focused = app.layout().focus;

        // A name with spaces: the shell would need it quoted, which is the
        // user's business — the insert must not mangle or requote it.
        let path = std::env::temp_dir().join("luvus insert/my notes.md");
        let expected = path.to_string_lossy().into_owned();
        assert!(
            expected.contains(' '),
            "the fixture is the interesting case"
        );

        assert_eq!(
            app.insert_path(&path),
            InsertPath::Inserted {
                target: focused,
                text: expected.clone(),
            },
            "the focused pane got the absolute path, spaces and all"
        );
    }

    /// Opening the FILES dock and its context menu must not move pane focus, so
    /// the pane `Insert Path` targets is still the one the user was typing in.
    ///
    /// This is the whole reason the action is worth having: reaching for the
    /// file tree to name a file would otherwise cost you the prompt you were
    /// naming it for.
    #[test]
    fn opening_the_files_menu_does_not_change_the_insert_target() {
        let _env = crate::persist::test_env("files-insert-target");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        app.split(Axis::Col);
        let typing_in = app.layout().focus;

        // Everything the user does to reach the action.
        app.toggle_files_dock();
        app.file_menu = Some(insert_menu(&std::env::temp_dir().join("target.rs")));
        assert_eq!(
            app.layout().focus,
            typing_in,
            "the dock and the menu left pane focus where it was"
        );

        let path = std::env::temp_dir().join("target.rs");
        assert!(
            matches!(
                app.insert_path(&path),
                InsertPath::Inserted { target, .. } if target == typing_in
            ),
            "the path went to the pane that had focus before the menu opened"
        );
    }

    /// Unix filenames may legally hold terminal controls. Newlines can submit
    /// the prompt, while Escape can terminate bracketed paste and make the
    /// remaining bytes act as input. Refuse the whole path rather than trim it:
    /// a silently shortened path is a wrong path.
    #[test]
    fn insert_path_refuses_terminal_control_characters() {
        let _env = crate::persist::test_env("files-insert-control");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();

        for bad in [
            "two\nlines.txt",
            "carriage\rreturn.txt",
            "tab\tname.txt",
            "escape\u{1b}[201~name.txt",
            "delete\u{7f}name.txt",
        ] {
            let path = std::env::temp_dir().join(bad);
            assert_eq!(
                app.insert_path(&path),
                InsertPath::ControlCharacter,
                "{bad:?} must not reach the terminal"
            );
        }

        // And a path with no control character still inserts, so the guard is
        // not simply refusing everything.
        let good = std::env::temp_dir().join("ordinary.txt");
        assert!(matches!(
            app.insert_path(&good),
            InsertPath::Inserted { .. }
        ));
    }

    /// A native read-only view is not a terminal: there is no prompt to insert
    /// into, so the action says so instead of quietly doing nothing.
    #[test]
    fn insert_path_reports_a_focused_leaf_that_is_not_a_pane() {
        let _env = crate::persist::test_env("files-insert-no-pane");
        let dir = std::env::temp_dir().join(format!("luvus-ip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("readme.md");
        std::fs::write(&file, b"hello\n").unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        // Focus a native file view rather than a terminal.
        app.open_file_view(file.clone(), OpenTarget::Tab);
        assert!(
            app.views.contains_key(&app.layout().focus),
            "the focused leaf is a native view, not a pane"
        );

        assert_eq!(
            app.insert_path(&file),
            InsertPath::NoPane,
            "a view has no prompt to insert into"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The menu row runs the action: picking `Insert Path` for a path that has
    /// to be refused produces the refusal, which nothing else in the menu does.
    #[test]
    fn the_insert_path_row_runs_the_action() {
        let _env = crate::persist::test_env("files-insert-row");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();

        app.file_menu = Some(insert_menu(&std::env::temp_dir().join("two\nlines.txt")));
        app.file_menu_action_pub(FileMenuItem::InsertPath);
        assert_eq!(
            app.toast.as_ref().map(|(text, _)| text.as_str()),
            Some("path contains a control character"),
            "the row reached insert_path and reported its refusal"
        );
    }

    /// The row is offered for folders too — a directory path is as useful to
    /// type as a file's — matching `Copy Path` rather than the open actions.
    #[test]
    fn insert_path_is_offered_for_files_and_folders() {
        for is_dir in [false, true] {
            let menu = FileMenu {
                path: PathBuf::from("/tmp/x"),
                is_dir,
                anchor: (0, 0),
                items: Vec::new(),
                editors: Vec::new(),
            };
            let items = menu.build_items();
            assert!(
                items.contains(&FileMenuItem::InsertPath),
                "Insert Path offered (is_dir={is_dir})"
            );
            let copy = items.iter().position(|i| *i == FileMenuItem::CopyPath);
            let insert = items.iter().position(|i| *i == FileMenuItem::InsertPath);
            assert!(copy < insert, "it sits with Copy Path, just below it");
        }
    }
}
