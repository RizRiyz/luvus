//! The folder picker — a modal to open (or create) a folder as a new **static
//! workspace** (workspace). The "+" button opens it: browse the filesystem, pick an
//! existing folder, or make a new one (which opens immediately). When the browsed
//! folder is a git repo it offers a second action row, **"Open with new
//! worktree"** (`w` also triggers it). The front door for workspaces and worktrees.

use std::path::{Path, PathBuf};

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
    /// Tab-completion cycle for [`FolderPicker::going_to`]. Cleared on edit/paste.
    pub(crate) go_to_cycle: Option<GoToCycle>,
    /// Generation the Go to field currently accepts completion scans for. Every
    /// edit and every directory change bumps it, so a scan still in flight is
    /// discarded on arrival instead of completing against text or a folder it
    /// was never listed for.
    pub(crate) go_to_generation: u64,
    /// Generation of the completion scan currently queued on the filesystem
    /// worker, if any. Keeps Tab to one bounded job, and carrying the generation
    /// means a late scan releases only its own slot, never a newer scan's.
    pub(crate) go_to_scanning: Option<u64>,
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
#[derive(Debug)]
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
/// rows and the footer hints remain interactive while inert modal space simply
/// keeps the picker open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickerHit {
    Row(usize),
    /// A footer key hint; a click behaves exactly like pressing that key.
    Hint(KeyCode),
    Modal,
}

/// Tab-completion cycle while the Go to field is active.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GoToCycle {
    /// Display parent, including the trailing `/` or `\` when the user typed one.
    parent: String,
    matches: Vec<String>,
    /// `None` while showing the shared prefix; `Some` once cycling names.
    index: Option<usize>,
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

fn unquote_go_to(input: &str) -> &str {
    let entered = input.trim();
    entered
        .strip_prefix('"')
        .and_then(|text| text.strip_suffix('"'))
        .or_else(|| {
            entered
                .strip_prefix('\'')
                .and_then(|text| text.strip_suffix('\''))
        })
        .unwrap_or(entered)
        .trim()
}

fn resolve_go_to_path(
    entered: &str,
    current: Option<&Path>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    if entered == "~" {
        home.map(Path::to_path_buf)
    } else if let Some(rest) = entered
        .strip_prefix("~/")
        .or_else(|| entered.strip_prefix("~\\"))
    {
        home.map(|home| home.join(rest))
    } else {
        let path = PathBuf::from(entered);
        Some(if path.is_absolute() {
            path
        } else {
            current.map(|c| c.join(&path)).unwrap_or(path)
        })
    }
}

fn chars_match_ignore_case(a: char, b: char) -> bool {
    a.to_lowercase().eq(b.to_lowercase())
}

fn names_equal_ignore_case(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

fn name_has_prefix_ignore_case(name: &str, prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    let mut chars = name.chars();
    for pc in prefix.chars() {
        match chars.next() {
            Some(nc) if chars_match_ignore_case(nc, pc) => {}
            _ => return false,
        }
    }
    true
}

fn common_name_prefix(names: &[String]) -> String {
    let Some(first) = names.first() else {
        return String::new();
    };
    let mut prefix: Vec<char> = first.chars().collect();
    for name in &names[1..] {
        let mut shared = Vec::new();
        for (a, b) in prefix.iter().copied().zip(name.chars()) {
            if chars_match_ignore_case(a, b) {
                shared.push(a);
            } else {
                break;
            }
        }
        prefix = shared;
        if prefix.is_empty() {
            break;
        }
    }
    prefix.into_iter().collect()
}

/// Separators the Go to field splits on. `\` counts only where the OS agrees:
/// on Unix it is an ordinary character in a directory name, so a folder literally
/// called `lit\folder` stays one component — which is how [`resolve_go_to_path`]
/// and Enter already treat it.
const PATH_SEPS: &[char] = if cfg!(windows) { &['/', '\\'] } else { &['/'] };

fn is_path_sep(c: char) -> bool {
    PATH_SEPS.contains(&c)
}

/// Length of a leading `~/` or `~\` home stem. Enter accepts both spellings on
/// every platform, so both are a component boundary here even where `\` is not
/// a separator.
fn home_stem_len(text: &str) -> usize {
    // Both spellings are ASCII, so this is always a character boundary.
    usize::from(text.starts_with("~/") || text.starts_with("~\\")) * 2
}

/// Delete the last path component, including a trailing separator.
/// `foo/bar/` and `foo/bar` both become `foo/`.
fn delete_last_path_word(buf: &mut String) {
    let mut chars: Vec<char> = buf.chars().collect();
    // Stop at a `~/` or `~\` stem while something still follows it, so deleting
    // a name does not silently turn a home path into a relative one. Once only
    // the stem is left it deletes like any other component (`~/` clears).
    let floor = match home_stem_len(buf) {
        stem if stem > 0 && chars.len() > stem => stem,
        _ => 0,
    };
    while chars.len() > floor && chars.last().copied().is_some_and(is_path_sep) {
        chars.pop();
    }
    while chars.len() > floor && chars.last().copied().is_some_and(|c| !is_path_sep(c)) {
        chars.pop();
    }
    *buf = chars.into_iter().collect();
}

fn is_word_delete_key(key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Backspace
            if key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
        {
            true
        }
        KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => true,
        _ => false,
    }
}

fn split_go_to_stem(entered: &str) -> (String, String, char) {
    // A leading `~/` (or `~\`, which Enter also accepts) is a stem, not a name:
    // skip it so the tilde is never treated as something to complete, and so the
    // remainder still resolves through `$HOME` on every platform.
    let home = home_stem_len(entered);
    match entered[home..].rfind(PATH_SEPS) {
        Some(offset) => {
            let i = home + offset;
            let sep = entered[i..].chars().next().unwrap_or('/');
            (
                entered[..=i].to_string(),
                entered[i + sep.len_utf8()..].to_string(),
                sep,
            )
        }
        None => (
            entered[..home].to_string(),
            entered[home..].to_string(),
            '/',
        ),
    }
}

/// Candidate names carried back to the app loop for cycling. Stepping through
/// more than this is not useful, and the list must stay bounded because a
/// pathological directory would otherwise hand the loop an unbounded `Vec`.
const MAX_GO_TO_MATCHES: usize = 512;

/// One bounded directory listing: the names kept for cycling, plus the prefix
/// shared by *every* match. The prefix is folded over all of them, including any
/// dropped past [`MAX_GO_TO_MATCHES`], so a truncated listing can still only
/// complete text the whole set agrees on.
struct GoToMatches {
    names: Vec<String>,
    common: String,
}

fn list_go_to_dirs(dir: &Path, typed_name: &str, show_hidden: bool) -> GoToMatches {
    let include_hidden = show_hidden || typed_name.starts_with('.');
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return GoToMatches {
            names: Vec::new(),
            common: String::new(),
        };
    };
    let mut names: Vec<String> = Vec::new();
    let mut extra: Option<String> = None;
    for entry in read_dir.filter_map(Result::ok) {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name == "." || name == ".." {
            continue;
        }
        if !include_hidden && name.starts_with('.') {
            continue;
        }
        if !name_has_prefix_ignore_case(&name, typed_name) {
            continue;
        }
        // Cheap name filters run first so only real candidates cost a `stat`.
        // `DirEntry::file_type` describes the link itself, which would hide a
        // symlinked directory that Enter happily navigates into; resolve the
        // joined path instead so Tab and Enter agree on what is a folder.
        if !dir.join(&name).is_dir() {
            continue;
        }
        if names.len() < MAX_GO_TO_MATCHES {
            names.push(name);
            continue;
        }
        // Past the cap the name is no longer offered, but it still constrains
        // what the shared prefix may claim.
        extra = Some(match extra {
            Some(prefix) => common_name_prefix(&[prefix, name]),
            None => name,
        });
    }
    names.sort_by(|a, b| {
        a.to_lowercase()
            .cmp(&b.to_lowercase())
            .then_with(|| a.cmp(b))
    });
    let mut common = common_name_prefix(&names);
    if let Some(extra) = extra {
        common = common_name_prefix(&[common, extra]);
    }
    GoToMatches { names, common }
}

fn cycle_go_to(cycle: &GoToCycle, reverse: bool) -> Option<(String, GoToCycle)> {
    if cycle.matches.is_empty() {
        return None;
    }
    let n = cycle.matches.len();
    let index = match cycle.index {
        None if reverse => n - 1,
        None => 0,
        Some(i) if reverse => (i + n - 1) % n,
        Some(i) => (i + 1) % n,
    };
    Some((
        format!("{}{}", cycle.parent, cycle.matches[index]),
        GoToCycle {
            parent: cycle.parent.clone(),
            matches: cycle.matches.clone(),
            index: Some(index),
        },
    ))
}

/// Immutable inputs for one Go to completion scan. Owned by the job so the
/// filesystem work borrows nothing from `App`.
pub(crate) struct GoToScan {
    input: String,
    current: PathBuf,
    home: Option<PathBuf>,
    show_hidden: bool,
    reverse: bool,
}

impl GoToScan {
    /// Runs on the filesystem worker: one `read_dir` of one directory, bounded by
    /// [`MAX_GO_TO_MATCHES`]. Touches nothing owned by the app loop.
    pub(crate) fn run(&self) -> Option<(String, Option<GoToCycle>)> {
        scan_go_to(
            &self.input,
            &self.current,
            self.home.as_deref(),
            self.show_hidden,
            self.reverse,
        )
    }
}

/// The filesystem half of completion: resolve the typed stem, list the one
/// directory it names, and decide what the field should show. Pure with respect
/// to `App`, so it is safe to run off the loop.
fn scan_go_to(
    input: &str,
    current: &Path,
    home: Option<&Path>,
    show_hidden: bool,
    reverse: bool,
) -> Option<(String, Option<GoToCycle>)> {
    let entered = unquote_go_to(input);
    if entered == "~" {
        return Some(("~/".to_string(), None));
    }

    let (parent, name, sep) = if entered.is_empty() {
        (String::new(), String::new(), '/')
    } else {
        split_go_to_stem(entered)
    };
    let list_dir = if parent.is_empty() {
        current.to_path_buf()
    } else {
        resolve_go_to_path(&parent, Some(current), home)?
    };
    if !list_dir.is_dir() {
        return None;
    }

    let GoToMatches { names, common } = list_go_to_dirs(&list_dir, &name, show_hidden);
    match names.len() {
        0 => None,
        1 => Some((format!("{}{}{sep}", parent, names[0]), None)),
        _ => {
            // Arm the cycle at the name the field is about to show, when that
            // text is itself one of the matches, so the next Tab steps past it
            // instead of redrawing it (`foo` alongside `foobar`). Otherwise the
            // shared prefix is not a directory of its own and cycling starts
            // from the top.
            let at = names
                .iter()
                .position(|m| names_equal_ignore_case(m, &common));
            let next_cycle = GoToCycle {
                parent: parent.clone(),
                matches: names,
                index: at,
            };
            if !names_equal_ignore_case(&name, &common) {
                Some((format!("{parent}{common}"), Some(next_cycle)))
            } else {
                let (text, next) = cycle_go_to(&next_cycle, reverse)?;
                Some((text, Some(next)))
            }
        }
    }
}

/// Whole-completion composition, kept for focused tests. Production splits the
/// two halves: cycling steps on the app loop, scanning runs on the worker.
#[cfg(test)]
fn complete_go_to(
    input: &str,
    current: &Path,
    home: Option<&Path>,
    show_hidden: bool,
    cycle: Option<&GoToCycle>,
    reverse: bool,
) -> Option<(String, Option<GoToCycle>)> {
    match cycle {
        Some(cycle) => {
            let (text, next) = cycle_go_to(cycle, reverse)?;
            Some((text, Some(next)))
        }
        None => scan_go_to(input, current, home, show_hidden, reverse),
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
    /// not a directory).
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
            go_to_cycle: None,
            go_to_generation: 0,
            go_to_scanning: None,
            error: None,
            is_repo: false,
            show_hidden: false,
        });
        self.picker_refresh();
    }

    pub fn close_folder_picker(&mut self) {
        // Retire the generation so a scan in flight cannot apply to a picker the
        // user reopens later.
        self.picker_go_to_generation = self.picker_go_to_generation.wrapping_add(1);
        self.picker = None;
    }

    /// Re-read the browsed path's entries (folders + files), dirs first.
    fn picker_refresh(&mut self) {
        // Any re-read means the browsed directory may have moved under an open Go
        // to field (row click, `..`, Home, hidden toggle). Completion candidates
        // describe the directory they were listed from, so retire them here — the
        // one choke point every navigation path already passes through.
        self.invalidate_go_to_completion();
        // Remember which entry the cursor highlights so filter changes (e.g.
        // `.` hiding dotfiles) re-anchor the selection by identity instead of
        // leaving it at a numeric index that may now point elsewhere.
        let selected = self.picker.as_ref().and_then(|p| match p.row(p.cursor) {
            Row::Entry(idx) => p.entries.get(idx).map(|e| e.name.clone()),
            _ => None,
        });
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
            if let Some(name) = selected {
                match p.entries.iter().position(|e| e.name == name) {
                    Some(pos) => p.cursor = p.leading() + pos,
                    // Highlighted entry was filtered out: fall back to the
                    // last fixed action row instead of letting the stale
                    // index land on some other directory.
                    None => p.cursor = p.leading() - 1,
                }
            }
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

    /// Consume a paste without letting it reach the pane behind the picker.
    /// Text sub-modes receive text; otherwise a pasted path navigates directly.
    pub fn picker_paste(&mut self, raw: &str) {
        let text: String = raw.chars().filter(|c| !c.is_control()).collect();
        if text.is_empty() {
            return;
        }
        if let Some(picker) = self.picker.as_mut() {
            if let Some(buffer) = picker.creating.as_mut() {
                buffer.push_str(&text);
                picker.error = None;
                return;
            }
            if let Some(buffer) = picker.going_to.as_mut() {
                buffer.push_str(&text);
                picker.error = None;
                self.invalidate_go_to_completion();
                return;
            }
        }
        self.picker_go_to(text);
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
                    _ if is_word_delete_key(key) => delete_last_path_word(buf),
                    KeyCode::Backspace => {
                        buf.pop();
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        buf.push(c);
                    }
                    _ => {}
                }
                return;
            }
        }
        if self.picker.as_ref().is_some_and(|p| p.going_to.is_some()) {
            match key.code {
                KeyCode::Esc => {
                    self.invalidate_go_to_completion();
                    if let Some(p) = self.picker.as_mut() {
                        p.going_to = None;
                        p.error = None;
                    }
                }
                KeyCode::Enter => {
                    let path = self
                        .picker
                        .as_ref()
                        .and_then(|p| p.going_to.clone())
                        .unwrap_or_default();
                    self.picker_go_to(path);
                }
                KeyCode::Tab | KeyCode::BackTab => {
                    let reverse = matches!(key.code, KeyCode::BackTab)
                        || key.modifiers.contains(KeyModifiers::SHIFT);
                    self.picker_complete_go_to(reverse);
                }
                _ if is_word_delete_key(key) => {
                    if let Some(p) = self.picker.as_mut() {
                        if let Some(buf) = p.going_to.as_mut() {
                            delete_last_path_word(buf);
                        }
                        p.error = None;
                    }
                    self.invalidate_go_to_completion();
                }
                KeyCode::Backspace => {
                    if let Some(p) = self.picker.as_mut() {
                        if let Some(buf) = p.going_to.as_mut() {
                            buf.pop();
                        }
                        p.error = None;
                    }
                    self.invalidate_go_to_completion();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(p) = self.picker.as_mut() {
                        if let Some(buf) = p.going_to.as_mut() {
                            buf.push(c);
                        }
                        p.error = None;
                    }
                    self.invalidate_go_to_completion();
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.picker_move(1),
            KeyCode::Char('k') | KeyCode::Up => self.picker_move(-1),
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => self.picker_up(),
            KeyCode::Right | KeyCode::Char('l') => self.picker_descend(),
            KeyCode::Enter => self.picker_activate(),
            KeyCode::Char('n') => {
                self.invalidate_go_to_completion();
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
        self.invalidate_go_to_completion();
        if let Some(p) = self.picker.as_mut() {
            p.creating = None;
            p.going_to = Some(String::new());
            p.error = None;
        }
    }

    /// Tab-complete the Go to buffer. Failed completion leaves the field unchanged.
    ///
    /// Stepping an already-listed cycle is pure in-memory work and stays on the
    /// loop, so held Tab never queues filesystem jobs. The first Tab for a stem
    /// needs a directory listing, which goes to the bounded worker instead of
    /// stalling the loop on a large or network-backed folder.
    fn picker_complete_go_to(&mut self, reverse: bool) {
        let Some(picker) = self.picker.as_ref() else {
            return;
        };
        let Some(input) = picker.going_to.as_deref() else {
            return;
        };
        if let Some(cycle) = picker.go_to_cycle.as_ref() {
            let stepped = cycle_go_to(cycle, reverse);
            if let Some((text, next)) = stepped {
                if let Some(p) = self.picker.as_mut() {
                    p.going_to = Some(text);
                    p.go_to_cycle = Some(next);
                    p.error = None;
                }
            }
            return;
        }
        // One scan at a time for the *current* text. Editing or navigating
        // retires the slot, so a superseded listing never gates the next Tab.
        if picker.go_to_scanning.is_some() {
            return;
        }
        let scan = GoToScan {
            input: input.to_string(),
            current: picker.path.clone(),
            home: crate::platform::home_dir(),
            show_hidden: picker.show_hidden,
            reverse,
        };
        let generation = self.begin_go_to_completion();
        let accepted = self.io_jobs.submit(self.app_tx.clone(), move || {
            let result = scan.run();
            Box::new(move |app| app.apply_go_to_completion(generation, result))
        });
        if accepted.is_err() {
            // A completion that cannot even be queued stays a no-op, like one
            // that finds nothing. Just free the slot for the next Tab.
            if let Some(p) = self.picker.as_mut() {
                p.go_to_scanning = None;
            }
        }
    }

    /// Claim the next completion generation and mark the scan pending. Separate
    /// from worker admission so focused tests can apply a deterministic result
    /// without scheduling a duplicate filesystem job.
    fn begin_go_to_completion(&mut self) -> u64 {
        self.picker_go_to_generation = self.picker_go_to_generation.wrapping_add(1);
        let generation = self.picker_go_to_generation;
        if let Some(p) = self.picker.as_mut() {
            p.go_to_generation = generation;
            p.go_to_scanning = Some(generation);
        }
        generation
    }

    /// Retire whatever completion the Go to field currently accepts: drops an
    /// armed cycle and fences any scan still in flight. Every edit and every
    /// directory change routes through here.
    ///
    /// The scan slot is released too, so Tab can immediately request completion
    /// for the new text instead of waiting on a listing whose result is already
    /// doomed — a stalled network mount would otherwise leave Tab dead. That is
    /// safe because the slot carries its generation: the abandoned scan can no
    /// longer match, so it cannot release the slot a later Tab is waiting on.
    /// A filesystem read cannot be cancelled, so the abandoned work still runs to
    /// completion on the worker; admission stays bounded by `IoJobs`.
    fn invalidate_go_to_completion(&mut self) {
        self.picker_go_to_generation = self.picker_go_to_generation.wrapping_add(1);
        let generation = self.picker_go_to_generation;
        if let Some(p) = self.picker.as_mut() {
            p.go_to_generation = generation;
            p.go_to_cycle = None;
            p.go_to_scanning = None;
        }
    }

    /// Apply a completion scan started by [`App::picker_complete_go_to`]. A
    /// result whose `generation` is not the one the field still waits on — closed,
    /// reopened, edited, or navigated since — is dropped, because its candidates
    /// describe text or a folder that is no longer on screen. Nothing here
    /// touches the filesystem.
    pub(crate) fn apply_go_to_completion(
        &mut self,
        generation: u64,
        result: Option<(String, Option<GoToCycle>)>,
    ) -> bool {
        let Some(p) = self.picker.as_mut() else {
            return false;
        };
        // Release the slot before validating, so a discarded result still lets
        // the next Tab start a scan. Only this scan's own slot is freed: a newer
        // one may already be queued against a later generation.
        if p.go_to_scanning == Some(generation) {
            p.go_to_scanning = None;
        }
        if p.go_to_generation != generation || p.going_to.is_none() {
            return false;
        }
        let Some((text, cycle)) = result else {
            return false;
        };
        p.going_to = Some(text);
        p.go_to_cycle = cycle;
        p.error = None;
        true
    }

    /// Resolve an entered path and browse to it. Absolute paths, paths relative
    /// to the currently browsed folder, and `~` / `~/...` are supported. A file
    /// path browses its parent directory without opening a workspace.
    fn picker_go_to(&mut self, input: String) {
        let entered = unquote_go_to(&input);
        if entered.is_empty() {
            let error = self.catalog.enter_folder_path.to_string();
            if let Some(p) = self.picker.as_mut() {
                p.error = Some(error);
            }
            return;
        }

        let current = self.picker.as_ref().map(|p| p.path.clone());
        let home = crate::platform::home_dir();
        let target = resolve_go_to_path(entered, current.as_deref(), home.as_deref());

        let target = target.and_then(|path| {
            if path.is_dir() {
                Some(path)
            } else if path.is_file() {
                path.parent().map(PathBuf::from)
            } else {
                None
            }
        });
        let Some(target) = target else {
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
        // Retires the completion generation along with the re-listing.
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
            go_to_cycle: None,
            go_to_generation: 0,
            go_to_scanning: None,
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
            go_to_cycle: None,
            go_to_generation: 0,
            go_to_scanning: None,
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

        // Selection survives `.` filter changes by identity, not index:
        // dotfiles sort before "readme.txt", so toggling shifts indices — the
        // highlight must stay on the same entry.
        let leading = app.picker.as_ref().unwrap().leading();
        let readme_idx = entries.iter().position(|e| e.name == "readme.txt").unwrap();
        app.picker.as_mut().unwrap().cursor = leading + readme_idx;
        app.handle_picker_key(KeyEvent::new(KeyCode::Char('.'), KeyModifiers::NONE));
        {
            let p = app.picker.as_ref().unwrap();
            match p.row(p.cursor) {
                Row::Entry(i) => assert_eq!(p.entries[i].name, "readme.txt"),
                other => panic!("expected readme.txt selected, got {:?}", other),
            }
        }
        // Cursor on a dotfile that gets filtered out → falls back to an
        // action row instead of silently landing on an unrelated directory.
        let secret_idx = app
            .picker
            .as_ref()
            .unwrap()
            .entries
            .iter()
            .position(|e| e.name == ".secret")
            .unwrap();
        app.picker.as_mut().unwrap().cursor = leading + secret_idx;
        app.handle_picker_key(KeyEvent::new(KeyCode::Char('.'), KeyModifiers::NONE));
        let p = app.picker.as_ref().unwrap();
        assert!(!matches!(p.row(p.cursor), Row::Entry(_)));

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
            .find_map(|(hit, rect)| (*hit == PickerHit::Hint(KeyCode::Char('g'))).then_some(*rect))
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

    /// A pasted path navigates the picker instead of leaking into the pane
    /// behind it — the only way to reach a deep folder without walking there.
    #[test]
    fn pasting_a_path_jumps_the_picker_there() {
        let tmp = std::env::temp_dir().join(format!("luvus-pickpaste-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let deep = tmp.join("a").join("b");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("f.txt"), "hi").unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.open_folder_picker();

        // Quoted, with trailing whitespace — what a file manager actually pastes.
        app.handle_event(crate::event::AppEvent::Paste(format!(
            "\"{}\"  ",
            deep.display()
        )));
        assert_eq!(
            app.picker.as_ref().unwrap().path,
            deep,
            "jumped to the path"
        );

        // A pasted *file* means its folder.
        app.handle_event(crate::event::AppEvent::Paste(
            deep.join("f.txt").display().to_string(),
        ));
        assert_eq!(app.picker.as_ref().unwrap().path, deep);

        // Nonsense reports itself and leaves the browsed folder alone.
        app.handle_event(crate::event::AppEvent::Paste("nope-not-a-path".into()));
        assert_eq!(app.picker.as_ref().unwrap().path, deep);
        assert!(app.picker.as_ref().unwrap().error.is_some());

        // A relative path resolves against the browsed folder — including a bare
        // filename, whose parent is the empty path and would otherwise strand
        // the picker on a folder that does not exist.
        app.handle_event(crate::event::AppEvent::Paste("f.txt".into()));
        assert_eq!(app.picker.as_ref().unwrap().path, deep);
        assert!(app.picker.as_ref().unwrap().error.is_none());
        app.picker.as_mut().unwrap().path = tmp.clone();
        app.picker_refresh();
        app.handle_event(crate::event::AppEvent::Paste("a/b".into()));
        assert_eq!(
            app.picker.as_ref().unwrap().path,
            deep,
            "walked down from here"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn paste_respects_picker_text_modes() {
        let _env = crate::persist::test_env("picker-paste-modes");
        let tmp = std::env::temp_dir().join(format!("luvus-pickmodes-{}", std::process::id()));
        let deep = tmp.join("nested");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&deep).unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.open_folder_picker_at(tmp.clone());

        app.picker_start_go_to();
        app.handle_event(AppEvent::Paste(format!("\"{}\"\n", deep.display())));
        let picker = app.picker.as_ref().unwrap();
        assert_eq!(picker.path, tmp, "paste only fills the Go To field");
        assert_eq!(
            picker.going_to.as_deref(),
            Some(format!("\"{}\"", deep.display()).as_str())
        );

        app.handle_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.picker.as_ref().unwrap().path, deep);

        app.handle_picker_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        app.handle_event(AppEvent::Paste("new\nfolder".into()));
        assert_eq!(
            app.picker.as_ref().unwrap().creating.as_deref(),
            Some("newfolder")
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    fn complete_fixture(label: &str) -> PathBuf {
        let tmp =
            std::env::temp_dir().join(format!("luvus-pickcomp-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        tmp
    }

    /// Take the next queued completion off the channel without applying it, so a
    /// test can change state first and prove the result is discarded.
    fn take_completion(rx: &std::sync::mpsc::Receiver<AppEvent>) -> io_jobs::Completion {
        loop {
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(AppEvent::IoCompleted(completion)) => return completion,
                Ok(_) => continue,
                Err(error) => panic!("completion never arrived: {error}"),
            }
        }
    }

    /// Drive one Tab the way the event loop does: the keypress queues a scan on
    /// the filesystem worker, and its completion applies on a later turn.
    fn press_tab(app: &mut App, rx: &std::sync::mpsc::Receiver<AppEvent>, key: KeyEvent) {
        let pending_before = app
            .picker
            .as_ref()
            .is_some_and(|p| p.go_to_scanning.is_some());
        app.handle_picker_key(key);
        let queued = app
            .picker
            .as_ref()
            .is_some_and(|p| p.go_to_scanning.is_some());
        if queued && !pending_before {
            take_completion(rx).apply(app);
        }
    }

    #[test]
    fn complete_go_to_unique_match_appends_separator_and_skips_files() {
        let tmp = complete_fixture("unique");
        std::fs::create_dir_all(tmp.join("Documents").join("src")).unwrap();
        std::fs::write(tmp.join("file.txt"), "x").unwrap();
        std::fs::write(tmp.join("Do-not"), "file").unwrap();

        let (text, cycle) = complete_go_to("Do", &tmp, None, false, None, false).unwrap();
        assert_eq!(text, "Documents/");
        assert!(cycle.is_none(), "unique match does not start a cycle");

        let (text, cycle) = complete_go_to("Documents/", &tmp, None, false, None, false).unwrap();
        assert_eq!(text, "Documents/src/");
        assert!(cycle.is_none());

        assert!(
            complete_go_to("file", &tmp, None, false, None, false).is_none(),
            "files are not completed"
        );
        assert_eq!(
            complete_go_to("\"Do\"", &tmp, None, false, None, false)
                .unwrap()
                .0,
            "Documents/"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn complete_go_to_extends_shared_prefix_then_cycles() {
        let tmp = complete_fixture("cycle");
        std::fs::create_dir_all(tmp.join("Documents")).unwrap();
        std::fs::create_dir_all(tmp.join("Downloads")).unwrap();

        let (text, cycle) = complete_go_to("D", &tmp, None, false, None, false).unwrap();
        assert_eq!(text, "Do");
        let cycle = cycle.expect("ambiguous names keep cycle state");

        let (text, cycle) = complete_go_to(&text, &tmp, None, false, Some(&cycle), false).unwrap();
        assert_eq!(text, "Documents");
        let cycle = cycle.unwrap();

        let (text, cycle) = complete_go_to(&text, &tmp, None, false, Some(&cycle), false).unwrap();
        assert_eq!(text, "Downloads");
        let cycle = cycle.unwrap();

        let (text, cycle) = complete_go_to(&text, &tmp, None, false, Some(&cycle), false).unwrap();
        assert_eq!(text, "Documents", "Tab wraps forward");
        let cycle = cycle.unwrap();

        let (text, _) = complete_go_to(&text, &tmp, None, false, Some(&cycle), true).unwrap();
        assert_eq!(text, "Downloads", "BackTab cycles backwards");

        let (text, _) = complete_go_to("Do", &tmp, None, false, None, true).unwrap();
        assert_eq!(text, "Downloads", "first BackTab starts at the last match");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn complete_go_to_keeps_tilde_stem_and_separator() {
        let home = complete_fixture("home");
        let current = complete_fixture("cwd");
        std::fs::create_dir_all(home.join("Documents")).unwrap();
        std::fs::create_dir_all(current.join("sub").join("docs")).unwrap();
        std::fs::create_dir_all(current.join("other")).unwrap();

        assert_eq!(
            complete_go_to("~", &current, Some(&home), false, None, false)
                .unwrap()
                .0,
            "~/"
        );
        assert_eq!(
            complete_go_to("~/Do", &current, Some(&home), false, None, false)
                .unwrap()
                .0,
            "~/Documents/"
        );
        assert_eq!(
            complete_go_to("sub/do", &current, None, false, None, false)
                .unwrap()
                .0,
            "sub/docs/"
        );

        let (text, cycle) = complete_go_to("", &current, None, false, None, false).unwrap();
        assert_eq!(text, "other");
        let cycle = cycle.expect("empty input cycles the current folder's children");
        let (text, _) = complete_go_to(&text, &current, None, false, Some(&cycle), false).unwrap();
        assert_eq!(text, "sub");

        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&current);
    }

    #[test]
    fn complete_go_to_hidden_dirs_follow_prefix_and_show_hidden() {
        let tmp = complete_fixture("hidden");
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::create_dir_all(tmp.join(".git")).unwrap();

        assert_eq!(
            complete_go_to("", &tmp, None, false, None, false)
                .unwrap()
                .0,
            "src/"
        );
        assert_eq!(
            complete_go_to(".", &tmp, None, false, None, false)
                .unwrap()
                .0,
            ".git/"
        );
        let (text, cycle) = complete_go_to("", &tmp, None, true, None, false).unwrap();
        assert_eq!(text, ".git");
        let cycle = cycle.unwrap();
        let (text, _) = complete_go_to(&text, &tmp, None, true, Some(&cycle), false).unwrap();
        assert_eq!(text, "src");

        assert!(complete_go_to("missing", &tmp, None, false, None, false).is_none());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn complete_go_to_is_case_insensitive() {
        let tmp = complete_fixture("case");
        std::fs::create_dir_all(tmp.join("Documents")).unwrap();
        assert_eq!(
            complete_go_to("doc", &tmp, None, false, None, false)
                .unwrap()
                .0,
            "Documents/"
        );
        assert_eq!(
            complete_go_to("DOC", &tmp, None, false, None, false)
                .unwrap()
                .0,
            "Documents/"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn delete_last_path_word_drops_one_component() {
        let mut buf = String::from("~/Documents/src/");
        delete_last_path_word(&mut buf);
        assert_eq!(buf, "~/Documents/");
        delete_last_path_word(&mut buf);
        assert_eq!(buf, "~/");
        delete_last_path_word(&mut buf);
        assert_eq!(buf, "");
        delete_last_path_word(&mut buf);
        assert_eq!(buf, "");

        buf = String::from("foo/bar");
        delete_last_path_word(&mut buf);
        assert_eq!(buf, "foo/");

        // `\` is a separator only where the OS agrees. On Unix it is part of the
        // name, so the whole thing is one component.
        buf = String::from(r"foo\bar\");
        delete_last_path_word(&mut buf);
        if cfg!(windows) {
            assert_eq!(buf, "foo\\");
        } else {
            assert_eq!(buf, "");
        }

        buf = String::from(r"keep/lit\folder");
        delete_last_path_word(&mut buf);
        if cfg!(windows) {
            assert_eq!(buf, "keep/lit\\", "`\\` is a boundary on Windows");
        } else {
            assert_eq!(buf, "keep/", "only `/` bounds a component on Unix");
        }

        // A `~\` stem is a boundary on either platform, because Enter accepts
        // that spelling everywhere. Deleting a name must not strand the user
        // without the home prefix they typed.
        buf = String::from(r"~\Documents");
        delete_last_path_word(&mut buf);
        assert_eq!(buf, "~\\");
        // The stem itself is still deletable once nothing follows it, exactly
        // like `~/`.
        delete_last_path_word(&mut buf);
        assert_eq!(buf, "");
    }

    #[test]
    fn go_to_alt_backspace_deletes_a_path_word() {
        let _env = crate::persist::test_env("picker-go-to-word-delete");
        let tmp = complete_fixture("word-delete");

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.open_folder_picker_at(tmp.clone());
        app.picker_start_go_to();
        for c in "~/Documents/src/".chars() {
            app.handle_picker_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }

        app.handle_picker_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT));
        assert_eq!(
            app.picker.as_ref().unwrap().going_to.as_deref(),
            Some("~/Documents/")
        );
        assert!(app.picker.as_ref().unwrap().go_to_cycle.is_none());

        app.handle_picker_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(app.picker.as_ref().unwrap().going_to.as_deref(), Some("~/"));

        app.handle_picker_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL));
        assert_eq!(app.picker.as_ref().unwrap().going_to.as_deref(), Some(""));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn go_to_tab_completes_folders_without_opening() {
        let _env = crate::persist::test_env("picker-go-to-tab");
        let tmp = complete_fixture("app-tab");
        std::fs::create_dir_all(tmp.join("Documents")).unwrap();
        std::fs::create_dir_all(tmp.join("Downloads")).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let workspaces_before = app.workspaces.len();
        app.open_folder_picker_at(tmp.clone());

        app.handle_picker_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(
            app.picker.as_ref().unwrap().going_to.is_none(),
            "Tab is inert while browsing"
        );
        assert!(
            app.picker.as_ref().unwrap().go_to_scanning.is_none(),
            "browsing queues no scan"
        );

        app.picker_start_go_to();
        app.handle_picker_key(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::NONE));
        press_tab(
            &mut app,
            &rx,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
        );
        assert_eq!(app.picker.as_ref().unwrap().going_to.as_deref(), Some("Do"));
        assert!(app.picker.as_ref().unwrap().go_to_cycle.is_some());

        // Stepping an armed cycle is in-memory only: no scan is queued, so these
        // presses need no completion pumped.
        app.handle_picker_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(
            app.picker.as_ref().unwrap().go_to_scanning.is_none(),
            "cycling stays off the worker"
        );
        assert_eq!(
            app.picker.as_ref().unwrap().going_to.as_deref(),
            Some("Documents")
        );
        app.handle_picker_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(
            app.picker.as_ref().unwrap().going_to.as_deref(),
            Some("Downloads")
        );

        app.handle_picker_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(
            app.picker.as_ref().unwrap().go_to_cycle.is_none(),
            "typing clears cycle state"
        );
        app.handle_picker_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        app.handle_event(AppEvent::Paste("s".into()));
        assert!(app.picker.as_ref().unwrap().go_to_cycle.is_none());

        let before = app.picker.as_ref().unwrap().going_to.clone();
        press_tab(
            &mut app,
            &rx,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
        );
        assert_eq!(
            app.picker.as_ref().unwrap().going_to,
            before,
            "failed completion is a no-op"
        );

        if let Some(buf) = app.picker.as_mut().unwrap().going_to.as_mut() {
            buf.clear();
            buf.push_str("Documents");
        }
        press_tab(
            &mut app,
            &rx,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
        );
        assert_eq!(
            app.picker.as_ref().unwrap().going_to.as_deref(),
            Some("Documents/")
        );
        app.handle_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.picker.as_ref().unwrap().path, tmp.join("Documents"));
        assert_eq!(app.workspaces.len(), workspaces_before);

        app.drain_io_jobs();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn go_to_footer_complete_hint_is_clickable() {
        use ratatui::backend::TestBackend;
        use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        use ratatui::Terminal;

        let _env = crate::persist::test_env("picker-go-to-tab-footer");
        let tmp = complete_fixture("footer");
        std::fs::create_dir_all(tmp.join("Documents")).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.open_folder_picker_at(tmp.clone());
        app.picker_start_go_to();
        app.handle_picker_key(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::NONE));

        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let screen: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("complete"));
        assert!(screen.contains("go to"));

        let tab = app
            .picker_rects
            .iter()
            .find_map(|(hit, rect)| (*hit == PickerHit::Hint(KeyCode::Tab)).then_some(*rect))
            .expect("Tab complete footer hit target");
        app.handle_event(AppEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: tab.x,
            row: tab.y,
            modifiers: KeyModifiers::NONE,
        }));
        // A clicked hint replays the key, so it queues the same scan Tab does.
        take_completion(&rx).apply(&mut app);
        assert_eq!(
            app.picker.as_ref().unwrap().going_to.as_deref(),
            Some("Documents/")
        );

        app.drain_io_jobs();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn navigating_drops_stale_go_to_completions() {
        let _env = crate::persist::test_env("picker-go-to-stale-cycle");
        let tmp = complete_fixture("stale-cycle");
        std::fs::create_dir_all(tmp.join("alpha")).unwrap();
        std::fs::create_dir_all(tmp.join("alphabet")).unwrap();
        std::fs::create_dir_all(tmp.join("zulu")).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();

        // Arm a cycle, then navigate by click while Go to is still open. The
        // candidates were listed from the previous directory, so they must go.
        let arm = |app: &mut App, rx: &std::sync::mpsc::Receiver<AppEvent>| {
            app.picker_start_go_to();
            for c in "al".chars() {
                app.handle_picker_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
            }
            press_tab(app, rx, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            assert_eq!(
                app.picker.as_ref().unwrap().going_to.as_deref(),
                Some("alpha")
            );
            assert!(app.picker.as_ref().unwrap().go_to_cycle.is_some());
        };

        app.open_folder_picker_at(tmp.clone());
        arm(&mut app, &rx);
        let zulu_row = {
            let p = app.picker.as_ref().unwrap();
            let idx = p.entries.iter().position(|e| e.name == "zulu").unwrap();
            p.leading() + idx
        };
        app.picker_click(zulu_row);
        let p = app.picker.as_ref().unwrap();
        assert_eq!(p.path, tmp.join("zulu"));
        assert!(
            p.go_to_cycle.is_none(),
            "a row click must not leave the old folder's candidates armed"
        );

        // The `..` row is the same story (row 2 outside a repo: open, home, up).
        app.open_folder_picker_at(tmp.clone());
        assert!(
            !app.picker.as_ref().unwrap().is_repo,
            "fixture is not a repo"
        );
        arm(&mut app, &rx);
        app.picker_click(2);
        let p = app.picker.as_ref().unwrap();
        assert_eq!(p.path, tmp.parent().unwrap());
        assert!(p.go_to_cycle.is_none(), "`..` invalidates the candidates");

        // So is re-listing the same folder under a different filter.
        app.open_folder_picker_at(tmp.clone());
        arm(&mut app, &rx);
        app.picker_toggle_hidden();
        assert!(
            app.picker.as_ref().unwrap().go_to_cycle.is_none(),
            "re-listing invalidates the candidates"
        );

        app.drain_io_jobs();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn go_to_treats_backslash_as_the_os_does() {
        let tmp = complete_fixture("backslash");
        let home = complete_fixture("backslash-home");
        std::fs::create_dir_all(home.join("Documents")).unwrap();

        // A `~/` stem is never a completion candidate itself, and `~\` — which
        // Enter already accepts — resolves through $HOME the same way.
        assert_eq!(
            complete_go_to("~/Do", &tmp, Some(&home), false, None, false)
                .unwrap()
                .0,
            "~/Documents/"
        );
        assert_eq!(
            complete_go_to("~\\Do", &tmp, Some(&home), false, None, false)
                .unwrap()
                .0,
            "~\\Documents/"
        );

        // On Unix `\` is a legal filename character, so a directory holding one
        // is a single component that completes and deletes as a whole.
        #[cfg(not(windows))]
        {
            std::fs::create_dir_all(tmp.join(r"lit\folder").join("inner")).unwrap();

            assert_eq!(
                complete_go_to("lit", &tmp, None, false, None, false)
                    .unwrap()
                    .0,
                "lit\\folder/"
            );
            assert_eq!(
                complete_go_to(r"lit\folder/in", &tmp, None, false, None, false)
                    .unwrap()
                    .0,
                "lit\\folder/inner/",
                "completion continues inside a backslash-named directory"
            );

            let mut buf = String::from(r"lit\folder");
            delete_last_path_word(&mut buf);
            assert_eq!(buf, "", "the backslash is part of the name, not a boundary");
        }

        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn complete_go_to_steps_past_an_exact_match() {
        let tmp = complete_fixture("exact");
        std::fs::create_dir_all(tmp.join("foo")).unwrap();
        std::fs::create_dir_all(tmp.join("foobar")).unwrap();
        std::fs::create_dir_all(tmp.join("foobaz")).unwrap();

        // `foo` is both the shared prefix and a real match, so the first Tab has
        // to move instead of redrawing the same text.
        let (text, cycle) = complete_go_to("foo", &tmp, None, false, None, false).unwrap();
        assert_eq!(text, "foobar");
        let cycle = cycle.unwrap();
        let (text, _) = complete_go_to(&text, &tmp, None, false, Some(&cycle), false).unwrap();
        assert_eq!(text, "foobaz");

        let (text, _) = complete_go_to("foo", &tmp, None, false, None, true).unwrap();
        assert_eq!(text, "foobaz", "BackTab steps the other way");

        // A shared prefix longer than the buffer still extends first — but since
        // `foo` is a real directory, the cycle is armed there so the Tab after
        // that steps on instead of redrawing `foo`.
        let (text, cycle) = complete_go_to("f", &tmp, None, false, None, false).unwrap();
        assert_eq!(text, "foo");
        let cycle = cycle.unwrap();
        let (text, _) = complete_go_to(&text, &tmp, None, false, Some(&cycle), false).unwrap();
        assert_eq!(text, "foobar");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn go_to_completion_runs_off_the_app_loop() {
        let _env = crate::persist::test_env("picker-go-to-offloop");
        let tmp = complete_fixture("offloop");
        std::fs::create_dir_all(tmp.join("Documents")).unwrap();
        std::fs::create_dir_all(tmp.join("Downloads")).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.open_folder_picker_at(tmp.clone());
        app.picker_start_go_to();
        app.handle_picker_key(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::NONE));

        // Tab returns before any directory is read: the keypress only queues the
        // scan, so the field is still exactly what the user typed.
        app.handle_picker_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let p = app.picker.as_ref().unwrap();
        assert_eq!(p.going_to.as_deref(), Some("D"));
        assert!(
            p.go_to_scanning.is_some(),
            "the scan is queued, not run inline"
        );
        assert!(p.go_to_cycle.is_none());

        // A second Tab while one scan is outstanding must not queue another.
        let generation = app.picker.as_ref().unwrap().go_to_generation;
        app.handle_picker_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(
            app.picker.as_ref().unwrap().go_to_generation,
            generation,
            "one scan at a time"
        );

        take_completion(&rx).apply(&mut app);
        let p = app.picker.as_ref().unwrap();
        assert_eq!(p.going_to.as_deref(), Some("Do"));
        assert!(
            p.go_to_scanning.is_none(),
            "the slot is released on completion"
        );
        assert!(p.go_to_cycle.is_some());

        app.drain_io_jobs();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn stale_go_to_scans_are_discarded() {
        let _env = crate::persist::test_env("picker-go-to-stale-scan");
        let tmp = complete_fixture("stale-scan");
        std::fs::create_dir_all(tmp.join("Documents")).unwrap();
        let other = tmp.join("other");
        std::fs::create_dir_all(other.join("Different")).unwrap();

        // Every one of these happens between the keypress and the completion, and
        // each must make the in-flight result inapplicable.
        for scenario in ["typed", "deleted", "navigated", "cancelled", "closed"] {
            let (tx, rx) = std::sync::mpsc::channel();
            let mut app = App::new(80, 24, tx).unwrap();
            app.open_folder_picker_at(tmp.clone());
            app.picker_start_go_to();
            app.handle_picker_key(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::NONE));
            app.handle_picker_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            assert!(app.picker.as_ref().unwrap().go_to_scanning.is_some());
            let completion = take_completion(&rx);

            match scenario {
                "typed" => {
                    app.handle_picker_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
                }
                "deleted" => {
                    app.handle_picker_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))
                }
                // Row 3 outside a repo is the first entry: `other`.
                "navigated" => {
                    let row = {
                        let p = app.picker.as_ref().unwrap();
                        let idx = p.entries.iter().position(|e| e.name == "other").unwrap();
                        p.leading() + idx
                    };
                    app.picker_click(row);
                }
                "cancelled" => {
                    app.handle_picker_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
                }
                "closed" => app.close_folder_picker(),
                _ => unreachable!(),
            }
            let before = app.picker.as_ref().and_then(|p| p.going_to.clone());

            assert!(
                !completion.apply(&mut app),
                "{scenario}: a superseded scan must not redraw the field"
            );
            match app.picker.as_ref() {
                Some(p) => {
                    assert_eq!(
                        p.going_to, before,
                        "{scenario}: field left as the user left it"
                    );
                    assert!(p.go_to_cycle.is_none(), "{scenario}: no candidates armed");
                    assert!(
                        p.go_to_scanning.is_none(),
                        "{scenario}: the slot is released anyway"
                    );
                }
                None => assert_eq!(scenario, "closed"),
            }

            // The released slot means the next Tab still works.
            if app.picker.is_some() {
                app.picker_start_go_to();
                app.handle_picker_key(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::NONE));
                press_tab(
                    &mut app,
                    &rx,
                    KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
                );
                let expected = match scenario {
                    "navigated" => "Different/",
                    _ => "Documents/",
                };
                assert_eq!(
                    app.picker.as_ref().unwrap().going_to.as_deref(),
                    Some(expected),
                    "{scenario}: completion recovers against the live folder"
                );
            }
            app.drain_io_jobs();
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn go_to_candidates_stay_bounded_without_overclaiming() {
        let tmp = complete_fixture("bounded");
        // More matches than the cap, all sharing `dir-`, so the shared prefix the
        // field may claim is decided by the whole set and not just the retained
        // slice. Two names below keep it from growing past `dir-`.
        for i in 0..(MAX_GO_TO_MATCHES + 64) {
            std::fs::create_dir_all(tmp.join(format!("dir-{i:04}"))).unwrap();
        }
        let listed = list_go_to_dirs(&tmp, "dir-", false);
        assert_eq!(
            listed.names.len(),
            MAX_GO_TO_MATCHES,
            "the list handed back to the loop is capped"
        );
        assert_eq!(
            listed.common, "dir-0",
            "names dropped past the cap still constrain the shared prefix"
        );

        // Completing must not invent text beyond what every match agrees on.
        let (text, cycle) = scan_go_to("dir-", &tmp, None, false, false).unwrap();
        assert_eq!(text, "dir-0");
        assert_eq!(cycle.unwrap().matches.len(), MAX_GO_TO_MATCHES);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_late_scan_does_not_release_a_newer_scans_slot() {
        let _env = crate::persist::test_env("picker-go-to-late-scan");
        let tmp = complete_fixture("late-scan");
        std::fs::create_dir_all(tmp.join("Documents")).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.open_folder_picker_at(tmp.clone());
        app.picker_start_go_to();
        app.handle_picker_key(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::NONE));

        // Queue one scan and hold its completion, then edit so it can never
        // apply. Editing retires its slot, which is what lets the next Tab run.
        app.handle_picker_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let first = take_completion(&rx);
        let first_slot = app
            .picker
            .as_ref()
            .unwrap()
            .go_to_scanning
            .expect("the first scan holds a slot");
        app.handle_picker_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert!(app.picker.as_ref().unwrap().go_to_scanning.is_none());

        // Queue a second scan, then deliver the first scan's late result. It must
        // neither write the field nor free the slot the second scan is waiting on.
        app.handle_picker_key(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::NONE));
        app.handle_picker_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let second_slot = app.picker.as_ref().unwrap().go_to_scanning;
        assert!(second_slot.is_some());
        assert_ne!(second_slot, Some(first_slot), "a fresh generation");

        assert!(!first.apply(&mut app));
        assert_eq!(
            app.picker.as_ref().unwrap().go_to_scanning,
            second_slot,
            "a late scan must not release the queued scan's slot"
        );
        assert_eq!(
            app.picker.as_ref().unwrap().going_to.as_deref(),
            Some("D"),
            "and must not write the field"
        );

        // The second scan still applies normally.
        take_completion(&rx).apply(&mut app);
        let p = app.picker.as_ref().unwrap();
        assert_eq!(p.going_to.as_deref(), Some("Documents/"));
        assert!(p.go_to_scanning.is_none());

        app.drain_io_jobs();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn an_obsolete_scan_does_not_gate_the_next_tab() {
        let _env = crate::persist::test_env("picker-go-to-supersede");
        let tmp = complete_fixture("supersede");
        std::fs::create_dir_all(tmp.join("Documents")).unwrap();
        std::fs::create_dir_all(tmp.join("Music")).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.open_folder_picker_at(tmp.clone());
        app.picker_start_go_to();
        app.handle_picker_key(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::NONE));
        app.handle_picker_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

        // Hold the first scan's completion: on a stalled mount this is the
        // listing that has not come back yet.
        let stalled = take_completion(&rx);
        let stalled_slot = app.picker.as_ref().unwrap().go_to_scanning;
        assert!(stalled_slot.is_some());

        // The user gives up on it and types something else. Tab must serve the
        // new text immediately rather than wait on the doomed listing.
        app.handle_picker_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert!(
            app.picker.as_ref().unwrap().go_to_scanning.is_none(),
            "editing retires the doomed scan's slot"
        );
        app.handle_picker_key(KeyEvent::new(KeyCode::Char('M'), KeyModifiers::NONE));
        press_tab(
            &mut app,
            &rx,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
        );
        assert_eq!(
            app.picker.as_ref().unwrap().going_to.as_deref(),
            Some("Music/"),
            "Tab is not gated on the abandoned scan"
        );

        // The abandoned listing finally lands and must change nothing.
        assert!(!stalled.apply(&mut app));
        assert_eq!(
            app.picker.as_ref().unwrap().going_to.as_deref(),
            Some("Music/")
        );

        app.drain_io_jobs();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    #[test]
    fn go_to_completes_symlinked_directories_like_enter_does() {
        let _env = crate::persist::test_env("picker-go-to-symlink");
        let tmp = complete_fixture("symlink");
        let target = tmp.join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, tmp.join("linked")).unwrap();
        std::os::unix::fs::symlink(tmp.join("nowhere"), tmp.join("dangling")).unwrap();
        std::fs::write(tmp.join("plain-file"), b"x").unwrap();
        std::os::unix::fs::symlink(tmp.join("plain-file"), tmp.join("linked-file")).unwrap();

        // A link to a directory completes: Enter resolves it with `is_dir`, so
        // refusing to complete it would make Tab disagree with Enter.
        assert_eq!(
            scan_go_to("link", &tmp, None, false, false).unwrap().0,
            "linked/"
        );
        // A link to a file, and one that resolves to nothing, are not folders.
        assert!(scan_go_to("linked-f", &tmp, None, false, false).is_none());
        assert!(scan_go_to("dang", &tmp, None, false, false).is_none());

        // And Enter agrees, which is the contract being matched.
        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.open_folder_picker_at(tmp.clone());
        app.picker_start_go_to();
        for c in "link".chars() {
            app.handle_picker_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        press_tab(
            &mut app,
            &rx,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
        );
        assert_eq!(
            app.picker.as_ref().unwrap().going_to.as_deref(),
            Some("linked/")
        );
        app.handle_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.picker.as_ref().unwrap().path, tmp.join("linked"));

        app.drain_io_jobs();
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
