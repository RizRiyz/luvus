//! The file-tree model (docs/38 FILE-1): a lazy, flat-indexed view of a folder
//! for the FILES sidebar dock.
//!
//! Two performance rules, both load-bearing:
//!
//! - **Only expanded directories are ever read.** Collapsing forgets nothing
//!   (the listing stays cached); expanding a never-seen dir asks the app to
//!   schedule an off-loop `read_dir`. A 100k-file repo costs what you expand.
//! - **Rendering is O(visible rows).** [`FileTree::visible_rows`] flattens only
//!   the *expanded* tree into a `Vec`, the same flat-index trick the git and
//!   orch lists use — no recursion per frame beyond the open depth, no hidden
//!   subtree cost.
//!
//! The model is pure: it never touches the filesystem itself. The app reads
//! directories on a worker thread and feeds them back via [`FileTree::apply_dir`].

pub mod preview;
mod view;
pub use view::{
    gutter_width, read_file, seg_text, selection_text, token_rows, view_text_w, wrap_ranges,
    FileLoad, FileView, SIZE_CAP,
};

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// One entry in a directory listing. Directories sort before files; within each
/// group, case-insensitive by name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
}

/// A cached directory listing.
#[derive(Clone, Default)]
struct Dir {
    entries: Vec<Entry>,
    /// False until an `apply_dir` has filled it — distinguishes "empty dir" from
    /// "not read yet", which drives lazy loading and the "loading…" affordance.
    loaded: bool,
}

/// One row of the flattened, on-screen tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisibleRow {
    pub path: PathBuf,
    pub name: String,
    pub depth: u16,
    pub is_dir: bool,
    /// A directory that is currently expanded (drives the `▾` vs `▸` chevron).
    pub expanded: bool,
    /// An expanded directory whose listing has not arrived yet.
    pub loading: bool,
}

/// What was open in one folder, parked while a different node is active.
///
/// The dock is a single tree shared by every node, so switching nodes re-roots
/// it. Discarding this state made the user lose their place: come back to a node
/// and every directory they had opened was collapsed again, with no record of
/// what they had been tracking.
#[derive(Default)]
struct TreeView {
    expanded: HashSet<PathBuf>,
    cursor: usize,
    scroll: usize,
}

/// The FILES dock's state: which folder, which dirs are open, and the cursor.
pub struct FileTree {
    root: PathBuf,
    dirs: HashMap<PathBuf, Dir>,
    expanded: HashSet<PathBuf>,
    /// Per-folder view state for nodes that are not currently active, keyed by
    /// root. Bounded by the number of folders visited this session, and each
    /// entry is a handful of paths, so it stays small.
    saved: HashMap<PathBuf, TreeView>,
    /// Directories whose read has been scheduled but not yet applied — so the
    /// app never schedules the same read twice.
    pending: HashSet<PathBuf>,
    /// Cursor + scroll into [`visible_rows`], clamped by the renderer.
    pub cursor: usize,
    pub scroll: usize,
    /// Show entries beginning with `.` (a display filter; `.git` is always
    /// hidden). Toggling it never re-reads — it only changes what is flattened.
    pub show_hidden: bool,
    /// Memoized flattened rows (perf): the renderer asks for these every frame,
    /// but the tree only changes on expand/collapse/read/re-root. `dirty` marks
    /// the cache stale; `cache_hidden` catches a `show_hidden` toggle. Rebuilt
    /// lazily on the next `visible_rows`, so an unchanged tree costs zero
    /// per-frame allocation.
    cache: Vec<VisibleRow>,
    dirty: bool,
    cache_hidden: bool,
}

impl FileTree {
    pub fn new(root: PathBuf) -> Self {
        FileTree {
            root,
            dirs: HashMap::new(),
            expanded: HashSet::new(),
            saved: HashMap::new(),
            pending: HashSet::new(),
            cursor: 0,
            scroll: 0,
            show_hidden: false,
            cache: Vec::new(),
            dirty: true,
            cache_hidden: false,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn root_loaded(&self) -> bool {
        self.is_loaded(&self.root)
    }

    /// Point the tree at a new folder (the active node changed).
    ///
    /// The outgoing folder's open directories, cursor and scroll are **parked**
    /// under its own root and restored if the user comes back, so switching
    /// nodes no longer collapses everything and loses their place. The expanded
    /// set still cannot simply carry over — a path from the old root would show
    /// under the wrong tree — but keying it by root gives correctness *and*
    /// continuity. The dir cache is shared and harmless to keep.
    pub fn set_root(&mut self, root: PathBuf) {
        if root == self.root {
            return;
        }
        let parked = TreeView {
            expanded: std::mem::take(&mut self.expanded),
            cursor: self.cursor,
            scroll: self.scroll,
        };
        let old = std::mem::replace(&mut self.root, root);
        // The tree starts rooted at nothing (`App::new`), which is not a folder
        // anyone returns to — don't keep a junk entry for it.
        if !old.as_os_str().is_empty() {
            self.saved.insert(old, parked);
        }
        let restored = self.saved.remove(&self.root).unwrap_or_default();
        self.expanded = restored.expanded;
        self.cursor = restored.cursor;
        self.scroll = restored.scroll;
        // Anything in flight was scheduled for the previous root; `needs_load`
        // re-requests whatever the restored view needs.
        self.pending.clear();
        self.dirty = true;
    }

    /// Directories that should be on screen but have not been read yet: the root
    /// plus every expanded dir, minus anything already loaded or in flight. The
    /// caller schedules an off-loop read for each and calls [`mark_pending`].
    pub fn needs_load(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let consider = |p: &Path, out: &mut Vec<PathBuf>| {
            if !self.is_loaded(p) && !self.pending.contains(p) {
                out.push(p.to_path_buf());
            }
        };
        consider(&self.root, &mut out);
        for p in &self.expanded {
            consider(p, &mut out);
        }
        out
    }

    pub fn mark_pending(&mut self, path: PathBuf) {
        self.pending.insert(path);
    }

    fn is_loaded(&self, path: &Path) -> bool {
        self.dirs.get(path).is_some_and(|d| d.loaded)
    }

    /// Fold a finished directory read into the cache. `entries` is stored as
    /// given, so the reader is responsible for the sort order (dirs first).
    ///
    /// A read that matches the cached listing byte-for-byte is dropped without
    /// marking the tree dirty, so the periodic rescan (which catches files
    /// created or removed outside luvus) costs nothing on screen when a folder
    /// hasn't changed.
    pub fn apply_dir(&mut self, path: PathBuf, entries: Vec<Entry>) {
        self.pending.remove(&path);
        if let Some(d) = self.dirs.get(&path) {
            if d.loaded && d.entries == entries {
                return;
            }
        }
        self.dirty = true;
        self.dirs.insert(
            path,
            Dir {
                entries,
                loaded: true,
            },
        );
    }

    /// Directories that are on screen right now and already read — the root plus
    /// every expanded, loaded directory. A periodic rescan re-reads exactly these
    /// to notice files created or deleted outside luvus, without ever descending
    /// into folders the user has collapsed (still O(what you can see)).
    pub fn loaded_visible_dirs(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if self.is_loaded(&self.root) {
            out.push(self.root.clone());
        }
        for p in &self.expanded {
            if self.is_loaded(p) {
                out.push(p.clone());
            }
        }
        out
    }

    /// Expand or collapse a directory. Collapsing also forgets its open
    /// descendants, so re-expanding a parent doesn't restore a deep open state
    /// the user can no longer see.
    pub fn toggle(&mut self, path: &Path) {
        self.dirty = true;
        if self.expanded.contains(path) {
            self.expanded.retain(|p| p != path && !p.starts_with(path));
        } else {
            self.expanded.insert(path.to_path_buf());
        }
    }

    /// Expand every ancestor directory of `path` so it becomes visible (docs/38
    /// `files.reveal`). The ancestors that are not yet read are picked up by the
    /// next `needs_load` sweep.
    pub fn reveal(&mut self, path: &Path) {
        let mut cur = path.parent();
        while let Some(dir) = cur {
            if !dir.starts_with(&self.root) && dir != self.root {
                break;
            }
            if dir != self.root {
                self.expanded.insert(dir.to_path_buf());
                self.dirty = true;
            }
            if dir == self.root {
                break;
            }
            cur = dir.parent();
        }
    }

    /// Forget every cached directory listing so the next sweep re-reads them —
    /// the `files.refresh` action. Expanded state is kept.
    pub fn invalidate(&mut self) {
        self.dirs.clear();
        self.pending.clear();
        self.dirty = true;
    }

    /// The flattened, on-screen tree — only expanded directories are descended.
    /// **Memoized**: the renderer calls this every frame, but it only rebuilds
    /// when the tree actually changed (expand/collapse/read/re-root) or
    /// `show_hidden` flipped. An unchanged tree returns the cached slice with no
    /// walk and no allocation.
    pub fn visible_rows(&mut self) -> &[VisibleRow] {
        if self.dirty || self.cache_hidden != self.show_hidden {
            self.cache = self.compute_rows();
            self.dirty = false;
            self.cache_hidden = self.show_hidden;
        }
        &self.cache
    }

    fn compute_rows(&self) -> Vec<VisibleRow> {
        let mut out = Vec::new();
        self.flatten(&self.root, 0, &mut out);
        out
    }

    fn flatten(&self, dir: &Path, depth: u16, out: &mut Vec<VisibleRow>) {
        let Some(d) = self.dirs.get(dir) else {
            return;
        };
        for e in &d.entries {
            if e.name == ".git" {
                continue;
            }
            if !self.show_hidden && e.name.starts_with('.') {
                continue;
            }
            let path = dir.join(&e.name);
            let expanded = e.is_dir && self.expanded.contains(&path);
            out.push(VisibleRow {
                path: path.clone(),
                name: e.name.clone(),
                depth,
                is_dir: e.is_dir,
                expanded,
                loading: expanded && !self.is_loaded(&path),
            });
            if expanded {
                self.flatten(&path, depth + 1, out);
            }
        }
    }
}

/// Read one directory into sorted [`Entry`]s: directories first, then files,
/// each case-insensitive by name. Runs on a worker thread (never the loop). An
/// unreadable directory yields an empty listing rather than an error, so a
/// permission-denied folder is simply empty instead of breaking the tree.
pub fn read_dir_entries(path: &Path) -> Vec<Entry> {
    let mut entries: Vec<Entry> = match std::fs::read_dir(path) {
        Ok(rd) => rd
            .flatten()
            .map(|de| {
                let name = de.file_name().to_string_lossy().into_owned();
                // `file_type` avoids a stat when the OS already knows; fall back
                // to `is_dir` via metadata only if the type is unknown.
                let is_dir = de
                    .file_type()
                    .map(|ft| ft.is_dir())
                    .unwrap_or_else(|_| de.path().is_dir());
                Entry { name, is_dir }
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(name: &str, is_dir: bool) -> Entry {
        Entry {
            name: name.into(),
            is_dir,
        }
    }

    #[test]
    fn only_expanded_dirs_are_flattened() {
        let root = PathBuf::from("/r");
        let mut t = FileTree::new(root.clone());
        t.apply_dir(root.clone(), vec![e("src", true), e("README.md", false)]);

        // Collapsed: src shows, its children do not.
        let rows = t.visible_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "src");
        assert!(rows[0].is_dir && !rows[0].expanded);
        assert_eq!(rows[1].name, "README.md");

        // Expanding src asks for a load; until it arrives the row reads "loading".
        t.toggle(&root.join("src"));
        assert_eq!(t.needs_load(), vec![root.join("src")]);
        let rows = t.visible_rows();
        assert!(rows[0].expanded && rows[0].loading);

        // Once the child listing arrives, its entries appear indented.
        t.apply_dir(root.join("src"), vec![e("mod.rs", false)]);
        let rows = t.visible_rows();
        assert_eq!(rows.len(), 3);
        assert_eq!((rows[1].name.as_str(), rows[1].depth), ("mod.rs", 1));
        assert!(!rows[0].loading);
    }

    #[test]
    fn collapsing_forgets_open_descendants() {
        let root = PathBuf::from("/r");
        let mut t = FileTree::new(root.clone());
        t.apply_dir(root.clone(), vec![e("a", true)]);
        t.apply_dir(root.join("a"), vec![e("b", true)]);
        t.toggle(&root.join("a"));
        t.toggle(&root.join("a/b"));
        // a expanded -> shows a, then b (expanded).
        assert_eq!(t.visible_rows().iter().filter(|r| r.expanded).count(), 2);

        t.toggle(&root.join("a")); // collapse the parent
        let rows = t.visible_rows();
        assert_eq!(rows.len(), 1, "only a shows, collapsed");
        assert!(!rows[0].expanded, "a is collapsed");
        // Re-expanding a must not restore b's open state (it was forgotten).
        t.toggle(&root.join("a"));
        let b = t
            .visible_rows()
            .iter()
            .find(|r| r.name == "b")
            .cloned()
            .unwrap();
        assert!(!b.expanded, "descendant open state forgotten");
    }

    #[test]
    fn hidden_filter_and_git_are_display_only() {
        let root = PathBuf::from("/r");
        let mut t = FileTree::new(root.clone());
        t.apply_dir(
            root.clone(),
            vec![e(".git", true), e(".env", false), e("main.rs", false)],
        );
        // .git is always hidden; dotfiles hidden by default.
        let names: Vec<_> = t.visible_rows().iter().map(|r| r.name.clone()).collect();
        assert_eq!(names, vec!["main.rs"]);
        // Toggling show_hidden reveals dotfiles without a re-read, but never .git.
        t.show_hidden = true;
        let names: Vec<_> = t.visible_rows().iter().map(|r| r.name.clone()).collect();
        assert_eq!(names, vec![".env", "main.rs"]);
    }

    #[test]
    fn set_root_shows_the_new_folder_not_the_old_one() {
        let mut t = FileTree::new(PathBuf::from("/r"));
        t.apply_dir(PathBuf::from("/r"), vec![e("x", true)]);
        t.toggle(&PathBuf::from("/r/x"));
        t.cursor = 5;
        t.set_root(PathBuf::from("/other"));
        assert_eq!(t.root(), Path::new("/other"));
        // A path from the old root must not show under the new one, and an
        // unvisited folder opens at the top.
        assert!(t.needs_load().contains(&PathBuf::from("/other")));
        assert!(!t.needs_load().contains(&PathBuf::from("/r/x")));
        assert_eq!(t.cursor, 0);
    }

    /// The reported bug: switching nodes collapsed everything, so coming back
    /// lost track of which files you had opened. The view is parked per folder
    /// and restored, while still never leaking a path across roots.
    #[test]
    fn returning_to_a_folder_restores_what_was_expanded() {
        let mut t = FileTree::new(PathBuf::from("/a"));
        t.apply_dir(PathBuf::from("/a"), vec![e("src", true), e("docs", true)]);
        t.toggle(&PathBuf::from("/a/src"));
        t.apply_dir(PathBuf::from("/a/src"), vec![e("deep", true)]);
        t.toggle(&PathBuf::from("/a/src/deep"));
        t.cursor = 3;
        t.scroll = 1;

        // Switch away: the other node starts clean, with nothing from /a open.
        t.set_root(PathBuf::from("/b"));
        assert_eq!(t.cursor, 0);
        assert_eq!(t.scroll, 0);
        assert!(!t.needs_load().contains(&PathBuf::from("/a/src")));

        // ...and switch back: everything the user had open is open again.
        t.set_root(PathBuf::from("/a"));
        let rows: Vec<_> = t.visible_rows().iter().map(|r| r.name.clone()).collect();
        assert!(
            rows.contains(&"deep".to_string()),
            "a nested expanded dir survives the round trip: {rows:?}"
        );
        assert_eq!(t.cursor, 3, "the cursor comes back too");
        assert_eq!(t.scroll, 1, "and so does the scroll position");
    }

    /// Two nodes must keep *separate* view state, not share one set. Asserted on
    /// each root's **child** rows, since a top-level entry is visible whether or
    /// not it is expanded and so would pass even with the state discarded.
    #[test]
    fn each_folder_keeps_its_own_expanded_state() {
        let mut t = FileTree::new(PathBuf::from("/a"));
        t.apply_dir(PathBuf::from("/a"), vec![e("x", true)]);
        t.toggle(&PathBuf::from("/a/x"));
        t.apply_dir(PathBuf::from("/a/x"), vec![e("in_a", false)]);

        t.set_root(PathBuf::from("/b"));
        t.apply_dir(PathBuf::from("/b"), vec![e("y", true)]);
        t.toggle(&PathBuf::from("/b/y"));
        t.apply_dir(PathBuf::from("/b/y"), vec![e("in_b", false)]);
        let rows: Vec<_> = t.visible_rows().iter().map(|r| r.name.clone()).collect();
        assert!(rows.contains(&"in_b".to_string()), "/b's own dir is open");
        assert!(
            !rows.contains(&"in_a".to_string()),
            "/a's state stays in /a"
        );

        t.set_root(PathBuf::from("/a"));
        let rows: Vec<_> = t.visible_rows().iter().map(|r| r.name.clone()).collect();
        assert!(rows.contains(&"in_a".to_string()), "/a's own state is back");
        assert!(
            !rows.contains(&"in_b".to_string()),
            "/b's state stays in /b"
        );
    }
}
