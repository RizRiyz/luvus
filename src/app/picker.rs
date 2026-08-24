//! The folder picker — a modal to open (or create) a folder as a new **static
//! workspace** (workspace). The "+" button opens it: browse the filesystem, pick an
//! existing folder, or make a new one (which opens immediately). When the browsed
//! folder is a git repo it offers a second action row, **"Open with new
//! worktree"** (`w` also triggers it). The front door for workspaces and worktrees.

use std::path::PathBuf;

use super::*;

/// One entry in the browsed directory — a subfolder (navigable) or a file
/// (shown so you can see the folder has content, but not selectable).
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
}

/// State of the open folder picker (workspace chooser).
pub struct FolderPicker {
    /// The directory currently being browsed.
    pub path: PathBuf,
    /// Folders + files in `path`, dirs first then files (dotfiles unless
    /// [`FolderPicker::show_hidden`]).
    pub entries: Vec<Entry>,
    /// Cursor into the row list (see [`Row`] / [`FolderPicker::row`]).
    pub cursor: usize,
    /// When making a new folder, the name being typed.
    pub creating: Option<String>,
    /// macOS-style "Go to" input. Enter navigates to this path but deliberately
    /// does not open it as a workspace; the OpenFolder row remains confirmation.
    pub going_to: Option<String>,
    /// Last filesystem error (e.g. permission denied), shown in the modal.
    pub error: Option<String>,
    /// Whether the browsed folder is a git repo — adds the "Open with new
    /// worktree" row (and the `w` accelerator). Recomputed when the path changes.
    pub is_repo: bool,
    /// Whether dotfile entries are listed (`.` toggles).
    pub show_hidden: bool,
}

/// A selectable row in the picker. The action rows lead; the directory entries
/// follow. The "open with worktree" row only exists when the folder is a repo.
pub enum Row {
    /// Open the browsed folder as a workspace.
    OpenFolder,
    /// Create a git worktree of the browsed repo (then open it).
    OpenWorktree,
    /// Jump to the user's home directory without opening it.
    Home,
    /// `..` — go to the parent directory.
    Up,
    /// `entries[idx]`.
    Entry(usize),
}

/// Mouse targets rendered by the picker. Modal is last in hit-test order so
/// rows and the Go-to footer remain interactive while inert modal space simply
/// keeps the picker open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickerHit {
    Row(usize),
    GoTo,
    ToggleHidden,
    Modal,
}

impl FolderPicker {
    /// Number of action rows before the directory entries: "open" + (optional)
    /// "open with worktree" + "home" + "..".
    fn leading(&self) -> usize {
        if self.is_repo {
            4
        } else {
            3
        }
    }

    /// Total selectable rows.
    pub fn row_count(&self) -> usize {
        self.leading() + self.entries.len()
    }

    /// Classify the row at index `i`.
    pub fn row(&self, i: usize) -> Row {
        match (i, self.is_repo) {
            (0, _) => Row::OpenFolder,
            (1, true) => Row::OpenWorktree,
            (1, false) | (2, true) => Row::Home,
            (2, false) | (3, true) => Row::Up,
            _ => Row::Entry(i - self.leading()),
        }
    }
}

impl App {
    /// Open the folder picker, starting in the active workspace's folder (or `$HOME`).
    pub fn open_folder_picker(&mut self) {
        let start = self
            .workspaces
            .get(self.active_ws)
            .map(|w| w.cwd.clone())
            .filter(|p| p.is_dir())
            .or_else(crate::platform::home_dir)
            .unwrap_or_else(|| PathBuf::from("/"));
        self.open_folder_picker_at(start);
    }

    /// Open the folder picker starting at `start` (falls back to `$HOME` if it's
    /// not a directory). Used by the workspace menu's "Open worktree".
    pub fn open_folder_picker_at(&mut self, start: PathBuf) {
        let start = start
            .is_dir()
            .then_some(start)
            .or_else(crate::platform::home_dir)
            .unwrap_or_else(|| PathBuf::from("/"));
        self.picker = Some(FolderPicker {
            path: start,
            entries: Vec::new(),
            cursor: 0,
            creating: None,
            going_to: None,
            error: None,
            is_repo: false,
            show_hidden: false,
        });
        self.picker_refresh();
    }

    pub fn close_folder_picker(&mut self) {
        self.picker = None;
    }

    /// Re-read the browsed path's entries (folders + files), dirs first.
    fn picker_refresh(&mut self) {
        if let Some(p) = self.picker.as_mut() {
            let mut entries: Vec<Entry> = std::fs::read_dir(&p.path)
                .map(|rd| {
                    rd.filter_map(Result::ok)
                        .filter_map(|e| {
                            let name = e.file_name().into_string().ok()?;
                            if !p.show_hidden && name.starts_with('.') {
                                return None;
                            }
                            let is_dir = e.file_type().map(|ty| ty.is_dir()).unwrap_or(false);
                            Some(Entry { name, is_dir })
                        })
                        .collect()
                })
                .unwrap_or_default();
            // Folders first, then files; each alphabetical (case-insensitive).
            entries.sort_by(|a, b| {
                b.is_dir
                    .cmp(&a.is_dir)
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            });
            p.entries = entries;
            p.is_repo = crate::git::local::is_repo(&p.path);
            p.cursor = p.cursor.min(p.row_count().saturating_sub(1));
        }
    }

    /// Show/hide dotfile entries (`.` or the footer hint).
    pub fn picker_toggle_hidden(&mut self) {
        if let Some(p) = self.picker.as_mut() {
            p.show_hidden = !p.show_hidden;
        }
        self.picker_refresh();
    }

    /// The "Open with new worktree" row (or `w`): create a git worktree of the
    /// browsed repo. Hands off to the branch prompt (targeting this folder), so
    /// the flow matches `Ctrl+Space G`.
    fn picker_make_worktree(&mut self) {
        let repo = self
            .picker
            .as_ref()
            .filter(|p| p.is_repo)
            .map(|p| p.path.clone());
        if let Some(repo) = repo {
            self.picker = None;
            self.worktree_repo = Some(repo);
            self.worktree_prompt = Some(String::new());
        }
    }

    /// Key handling while the folder picker is open.
    pub fn handle_picker_key(&mut self, key: KeyEvent) {
        // New-folder name input sub-mode.
        if let Some(p) = self.picker.as_mut() {
            if let Some(buf) = p.creating.as_mut() {
                match key.code {
                    KeyCode::Esc => {
                        p.creating = None;
                        p.error = None;
                    }
                    KeyCode::Enter => {
                        let name = buf.clone();
                        self.picker_create_folder(name);
                    }
                    KeyCode::Backspace => {
                        buf.pop();
                    }
                    KeyCode::Char(c) => buf.push(c),
                    _ => {}
                }
                return;
            }
            if let Some(buf) = p.going_to.as_mut() {
                match key.code {
                    KeyCode::Esc => {
                        p.going_to = None;
                        p.error = None;
                    }
                    KeyCode::Enter => {
                        let path = buf.clone();
                        self.picker_go_to(path);
                    }
                    KeyCode::Backspace => {
                        buf.pop();
                        p.error = None;
                    }
                    KeyCode::Char(c) => {
                        buf.push(c);
                        p.error = None;
                    }
                    _ => {}
                }
                return;
            }
        }
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.picker_move(1),
            KeyCode::Char('k') | KeyCode::Up => self.picker_move(-1),
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => self.picker_up(),
            KeyCode::Right | KeyCode::Char('l') => self.picker_descend(),
            KeyCode::Enter => self.picker_activate(),
            KeyCode::Char('n') => {
                if let Some(p) = self.picker.as_mut() {
                    p.creating = Some(String::new());
                    p.going_to = None;
                    p.error = None;
                }
            }
            KeyCode::Char('g') => self.picker_start_go_to(),
            KeyCode::Char('.') => self.picker_toggle_hidden(),
            KeyCode::Home | KeyCode::Char('~') => self.picker_home(),
            KeyCode::Char('w') => self.picker_make_worktree(),
            KeyCode::Esc | KeyCode::Char('q') => self.close_folder_picker(),
            _ => {}
        }
    }

    fn picker_move(&mut self, delta: i32) {
        if let Some(p) = self.picker.as_mut() {
            let max = p.row_count().saturating_sub(1) as i32;
            p.cursor = (p.cursor as i32 + delta).clamp(0, max) as usize;
        }
    }

    /// Wheel-scroll the browse list by `delta` rows (cursor stays in view).
    pub fn picker_scroll(&mut self, delta: i32) {
        self.picker_move(delta);
    }

    /// Browse up to the parent directory.
    fn picker_up(&mut self) {
        if let Some(p) = self.picker.as_mut() {
            if let Some(parent) = p.path.parent() {
                p.path = parent.to_path_buf();
                p.cursor = 0;
            }
        }
        self.picker_refresh();
    }

    /// Browse the home directory without opening a workspace.
    fn picker_home(&mut self) {
        let Some(home) = crate::platform::home_dir().filter(|path| path.is_dir()) else {
            let error = self.catalog.home_unavailable.to_string();
            if let Some(p) = self.picker.as_mut() {
                p.error = Some(error);
            }
            return;
        };
        if let Some(p) = self.picker.as_mut() {
            p.path = home;
            p.cursor = 0;
            p.error = None;
        }
        self.picker_refresh();
    }

    /// Start the in-modal path navigator. It is intentionally separate from
    /// opening a workspace so Enter cannot accidentally confirm a folder.
    pub fn picker_start_go_to(&mut self) {
        if let Some(p) = self.picker.as_mut() {
            p.creating = None;
            p.going_to = Some(String::new());
            p.error = None;
        }
    }

    /// Resolve an entered path and browse to it. Absolute paths, paths relative
    /// to the currently browsed folder, and `~` / `~/...` are supported.
    fn picker_go_to(&mut self, input: String) {
        let entered = input.trim();
        if entered.is_empty() {
            let error = self.catalog.enter_folder_path.to_string();
            if let Some(p) = self.picker.as_mut() {
                p.error = Some(error);
            }
            return;
        }

        let current = self.picker.as_ref().map(|p| p.path.clone());
        let target = if entered == "~" {
            crate::platform::home_dir()
        } else if let Some(rest) = entered
            .strip_prefix("~/")
            .or_else(|| entered.strip_prefix("~\\"))
        {
            crate::platform::home_dir().map(|home| home.join(rest))
        } else {
            let path = PathBuf::from(entered);
            Some(if path.is_absolute() {
                path
            } else {
                current.unwrap_or_default().join(path)
            })
        };

        let Some(target) = target.filter(|path| path.is_dir()) else {
            let error = format!("{}: {entered}", self.catalog.folder_not_found);
            if let Some(p) = self.picker.as_mut() {
                p.error = Some(error);
            }
            return;
        };

        if let Some(p) = self.picker.as_mut() {
            p.path = target;
            p.cursor = 0;
            p.going_to = None;
            p.error = None;
        }
        self.picker_refresh();
    }

    /// Browse into the highlighted subdirectory (only folder entries navigate).
    fn picker_descend(&mut self) {
        let target = self.picker.as_ref().and_then(|p| match p.row(p.cursor) {
            Row::Entry(idx) => p
                .entries
                .get(idx)
                .filter(|e| e.is_dir)
                .map(|e| p.path.join(&e.name)),
            _ => None,
        });
        if let Some(t) = target {
            if let Some(p) = self.picker.as_mut() {
                p.path = t;
                p.cursor = 0;
            }
            self.picker_refresh();
        }
    }

    /// `⏎` / click — contextual on the highlighted row.
    pub fn picker_activate(&mut self) {
        let Some(row) = self.picker.as_ref().map(|p| p.row(p.cursor)) else {
            return;
        };
        match row {
            // Open the current folder as a new static workspace.
            Row::OpenFolder => {
                if let Some(p) = self.picker.take() {
                    self.create_workspace_at(p.path);
                }
            }
            Row::OpenWorktree => self.picker_make_worktree(),
            Row::Home => self.picker_home(),
            Row::Up => self.picker_up(),
            Row::Entry(_) => self.picker_descend(),
        }
    }

    /// Click a picker row (sets the cursor, then acts on it).
    pub fn picker_click(&mut self, row: usize) {
        if let Some(p) = self.picker.as_mut() {
            if row < p.row_count() {
                p.cursor = row;
            }
        }
        self.picker_activate();
    }

    fn picker_create_folder(&mut self, name: String) {
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }
        let Some(p) = self.picker.as_mut() else {
            return;
        };
        let new = p.path.join(&name);
        if let Err(e) = std::fs::create_dir(&new) {
            p.error = Some(e.to_string());
            return;
        }
        // Open the brand-new folder as a workspace straight away — making a folder from
        // the workspace picker means "use this as my workspace", so don't make the
        // user then hunt for "open this folder".
        self.picker = None;
        self.create_workspace_at(new);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_adds_an_open_with_worktree_row_that_shifts_the_indices() {
        let mut p = FolderPicker {
            path: PathBuf::from("/x"),
            entries: vec![Entry {
                name: "a".into(),
                is_dir: true,
            }],
            cursor: 0,
            creating: None,
            going_to: None,
            error: None,
            is_repo: false,
            show_hidden: false,
        };
        // Plain folder: [Open] [Home] [..] [a]
        assert_eq!(p.row_count(), 4);
        assert!(matches!(p.row(0), Row::OpenFolder));
        assert!(matches!(p.row(1), Row::Home));
        assert!(matches!(p.row(2), Row::Up));
        assert!(matches!(p.row(3), Row::Entry(0)));

        // Git repo: the worktree row appears at 1 and pushes the rest down.
        p.is_repo = true;
        assert_eq!(p.row_count(), 5);
        assert!(matches!(p.row(0), Row::OpenFolder));
        assert!(matches!(p.row(1), Row::OpenWorktree));
        assert!(matches!(p.row(2), Row::Home));
        assert!(matches!(p.row(3), Row::Up));
        assert!(matches!(p.row(4), Row::Entry(0)));
    }

    #[test]
    fn selecting_the_worktree_row_opens_the_branch_prompt() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.picker = Some(FolderPicker {
            path: PathBuf::from("/tmp/some-repo"),
            entries: Vec::new(),
            cursor: 1, // the "Open with new worktree" row
            creating: None,
            going_to: None,
            error: None,
            is_repo: true,
            show_hidden: false,
        });
        app.picker_activate(); // ⏎ / click on that row
        assert!(app.picker.is_none(), "picker closes");
        assert!(app.worktree_prompt.is_some(), "branch prompt opens");
        assert_eq!(app.worktree_repo, Some(PathBuf::from("/tmp/some-repo")));
    }

    #[test]
    fn picker_browses_and_opens_a_folder() {
        let tmp = std::env::temp_dir().join(format!("luvus-picker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("sub")).unwrap();
        std::fs::write(tmp.join("readme.txt"), "hi").unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let workspaces_before = app.workspaces.len();

        app.open_folder_picker();
        // Point the picker at our temp dir and refresh.
        app.picker.as_mut().unwrap().path = tmp.clone();
        app.picker_refresh();
        let entries = &app.picker.as_ref().unwrap().entries;
        // Folders and files both show; the folder sorts before the file.
        assert!(entries.iter().any(|e| e.name == "sub" && e.is_dir));
        assert!(entries.iter().any(|e| e.name == "readme.txt" && !e.is_dir));
        assert!(entries[0].is_dir, "directories are listed before files");

        // Dotfiles are hidden by default; `.` toggles them on and back off.
        std::fs::write(tmp.join(".secret"), "x").unwrap();
        app.picker_refresh();
        assert!(!app.picker.as_ref().unwrap().show_hidden);
        app.handle_picker_key(KeyEvent::new(KeyCode::Char('.'), KeyModifiers::NONE));
        let entries = &app.picker.as_ref().unwrap().entries;
        assert!(app.picker.as_ref().unwrap().show_hidden);
        assert!(entries.iter().any(|e| e.name == ".secret"));
        app.handle_picker_key(KeyEvent::new(KeyCode::Char('.'), KeyModifiers::NONE));
        let entries = &app.picker.as_ref().unwrap().entries;
        assert!(!app.picker.as_ref().unwrap().show_hidden);
        assert!(!entries.iter().any(|e| e.name == ".secret"));

        // Cursor 0 = "use this folder" → opens the browsed folder as a workspace.
        app.picker.as_mut().unwrap().cursor = 0;
        app.handle_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.picker.is_none(), "picker closed after opening");
        assert_eq!(
            app.workspaces.len(),
            workspaces_before + 1,
            "a workspace was created"
        );
        assert_eq!(app.workspaces.last().unwrap().cwd, tmp);

        // Reopen and make a new folder: it opens as a workspace immediately (one step).
        app.open_folder_picker();
        app.picker.as_mut().unwrap().path = tmp.clone();
        app.picker_refresh();
        app.handle_picker_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        for c in "fresh".chars() {
            app.handle_picker_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        app.handle_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(tmp.join("fresh").is_dir(), "new folder created");
        assert!(
            app.picker.is_none(),
            "new folder opens as a workspace (no second Enter)"
        );
        assert_eq!(app.workspaces.len(), workspaces_before + 2);
        assert_eq!(app.workspaces.last().unwrap().cwd, tmp.join("fresh"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn go_to_browses_a_path_without_opening_it() {
        let _env = crate::persist::test_env("picker-go-to");
        let tmp = std::env::temp_dir().join(format!("luvus-picker-go-{}", std::process::id()));
        let target = tmp.join("nested");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&target).unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let workspaces_before = app.workspaces.len();
        app.open_folder_picker_at(tmp.clone());

        app.handle_picker_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        assert_eq!(app.picker.as_ref().unwrap().going_to.as_deref(), Some(""));
        for c in target.display().to_string().chars() {
            app.handle_picker_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        app.handle_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let picker = app.picker.as_ref().expect("navigation keeps picker open");
        assert_eq!(picker.path, target);
        assert!(
            picker.going_to.is_none(),
            "successful navigation exits input"
        );
        assert_eq!(
            app.workspaces.len(),
            workspaces_before,
            "Go to must not open a workspace"
        );

        // Explicit confirmation is still required.
        app.handle_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.picker.is_none());
        assert_eq!(app.workspaces.len(), workspaces_before + 1);
        assert_eq!(app.workspaces.last().unwrap().cwd, target);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn go_to_keeps_invalid_paths_editable() {
        let _env = crate::persist::test_env("picker-go-to-invalid");
        let tmp =
            std::env::temp_dir().join(format!("luvus-picker-go-invalid-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.open_folder_picker_at(tmp.clone());
        app.picker_start_go_to();
        for c in "missing".chars() {
            app.handle_picker_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        app.handle_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let picker = app.picker.as_ref().unwrap();
        assert_eq!(picker.path, tmp, "failed navigation keeps current folder");
        assert_eq!(picker.going_to.as_deref(), Some("missing"));
        assert!(picker.error.is_some());

        app.handle_picker_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert!(app.picker.as_ref().unwrap().error.is_none());
        app.handle_picker_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let picker = app.picker.as_ref().expect("Escape only closes Go to input");
        assert!(picker.going_to.is_none());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn home_row_and_go_to_footer_are_interactive() {
        use ratatui::backend::TestBackend;
        use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        use ratatui::Terminal;

        let _env = crate::persist::test_env("picker-home-and-footer");
        let tmp = std::env::temp_dir().join(format!("luvus-picker-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let workspaces_before = app.workspaces.len();
        app.open_folder_picker_at(tmp.clone());

        let home_row = (0..app.picker.as_ref().unwrap().row_count())
            .find(|&i| matches!(app.picker.as_ref().unwrap().row(i), Row::Home))
            .unwrap();
        app.picker.as_mut().unwrap().cursor = home_row;
        app.picker_activate();
        assert_eq!(
            app.picker.as_ref().unwrap().path,
            crate::platform::home_dir().unwrap()
        );
        assert_eq!(app.workspaces.len(), workspaces_before);

        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let screen: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("Home"));
        assert!(screen.contains("go to"));

        let modal = app
            .picker_rects
            .iter()
            .find_map(|(hit, rect)| (*hit == PickerHit::Modal).then_some(*rect))
            .expect("modal hit target");
        app.handle_event(AppEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: modal.x,
            row: modal.y,
            modifiers: KeyModifiers::NONE,
        }));
        assert!(app.picker.is_some(), "clicking modal chrome keeps it open");

        let go_to = app
            .picker_rects
            .iter()
            .find_map(|(hit, rect)| (*hit == PickerHit::GoTo).then_some(*rect))
            .expect("Go to footer hit target");
        app.handle_event(AppEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: go_to.x,
            row: go_to.y,
            modifiers: KeyModifiers::NONE,
        }));
        assert!(app.picker.as_ref().unwrap().going_to.is_some());

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
