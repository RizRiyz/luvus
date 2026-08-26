//! Application state: workspaces → tabs → a BSP tree of panes, plus per-pane
//! agent detection. Panes are stored flat and referenced by id from the tree
//! (docs/04). Prefix-key driven.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{mpsc::Sender, Arc};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use serde_json::{json, Value};

use crate::detect;
use crate::event::AppEvent;
use crate::ids::PaneId;
use crate::ipc::api::{self, ApiRequest, EventBus};
use crate::layout::{Axis, Dir, TileLayout};
use crate::module::context::Target;
use crate::persist::{self, SessionSnapshot};
use crate::terminal::pty::Pane;
use crate::ui::theme::{State, Theme};

mod backend;
mod board;
pub use board::agent_choices;
pub(crate) mod diff;
mod dispatch;
pub(crate) mod files;
mod git;
mod input;
mod keys;
mod mission;
mod modules;
mod picker;
mod search;
mod settings;
mod switcher;

pub use search::{GlobalSearch, SearchFlash};

pub use keys::{key_reference_rows, presets, Cmd, PrefixSpec, KEY_REFERENCE};
pub use modules::ModuleMenuAction;
pub use picker::{FolderPicker, PickerHit, Row};
pub use settings::{
    GeneralRow, LayoutRow, ModuleRow, SettingsTab, SettingsUi, KEYS_HEADER_ROWS, KEYS_PREFIX_ROW,
    KEYS_PRESET_ROW,
};

/// How recently a pane must have produced PTY output to read as *raw* Working.
const ACTIVITY_WINDOW: Duration = Duration::from_millis(700);

/// Anti-jitter dwell: how long a pane must stay *quiet* before its published
/// status is allowed to fall back to Idle/Done. Agents stream in bursts — a
/// single turn has natural gaps (thinking, tool calls, API latency) far longer
/// than `ACTIVITY_WINDOW` — so without this the status flaps Working↔Idle↔Done
/// many times per turn. Transitions *into* an active state (Working/Blocked)
/// are not delayed, so the sidebar still reacts instantly; only the fall back to
/// quiet is debounced. See `detect_tick` and docs/07.
const QUIET_DWELL: Duration = Duration::from_millis(2500);

/// After a pane's PTY is resized, its grid is transiently unreliable — it reflows
/// old rows into view and the agent then repaints its whole screen, which can
/// surface a stale spinner/hint line in the detection region for a tick or two.
/// Detection is frozen this long after a resize so switching to a tab (whose panes
/// have a different geometry) can't flip an idle agent to a lingering "working".
/// Kept short so a genuinely working agent still lights up promptly after a resize.
const RESIZE_GRACE: Duration = Duration::from_millis(450);

/// Sidebar width in columns. `sidebar_width` is adjustable at runtime and in the
/// Settings → Layout tab; these bound it. Colors come from the `Theme`, also
/// selectable in Settings → Theme (see docs/15).
pub const SIDEBAR_WIDTH_DEFAULT: u16 = 26;
pub const SIDEBAR_WIDTH_MIN: u16 = 18;
pub const SIDEBAR_WIDTH_MAX: u16 = 44;

/// The most docks a single sidebar may hold (docs/29). A side that is full rejects
/// further placements — the dock stays where it was and the user rearranges in
/// Settings → Layout. Overflow in a loaded config is truncated to "off". (If this
/// changes, the `sidebar_full` toast copy in `i18n` stays number-free on purpose.)
pub const MAX_DOCKS_PER_SIDE: usize = 3;

/// A relocatable sidebar section (docs/29). Built-ins are `Workspaces` and
/// `Agents`; `Module` is reserved for extension-contributed docks (DOCK-4).
/// Deliberately distinct from a *pane* (a terminal tile).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DockKind {
    Workspaces,
    Agents,
    /// The native file tree of the active node (docs/38).
    Files,
    Module(String),
}

impl DockKind {
    /// Stable id used in `config.json` and the UHP.
    pub fn id(&self) -> &str {
        match self {
            DockKind::Workspaces => "workspaces",
            DockKind::Agents => "agents",
            DockKind::Files => "files",
            DockKind::Module(id) => id,
        }
    }

    /// Parse a config/API id back into a built-in dock. Module ids resolve to
    /// `Module(id)`; the caller validates against installed modules.
    pub fn from_id(id: &str) -> DockKind {
        match id {
            "workspaces" => DockKind::Workspaces,
            "agents" => DockKind::Agents,
            "files" => DockKind::Files,
            other => DockKind::Module(other.to_string()),
        }
    }
}

/// A non-PTY leaf renderer (docs/38 FILE-3). The tile tree holds the leaf id;
/// this holds what to draw there. Deliberately an enum so the diff viewer
/// (docs/30) can add its own variant later without another seam.
pub enum ViewKind {
    File(crate::files::FileView),
    Diff(Box<crate::diff::DiffView>),
}

/// Which sidebar a dock lives in (docs/29).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    Left,
    Right,
}

/// One row a module pushes into its dock (docs/29, DOCK-4). `dot` is an optional
/// state name (`working`/`blocked`/`done`/`idle`) rendered as a coloured dot;
/// `action` is a module action id invoked when the row is clicked.
#[derive(Clone)]
pub struct DockRow {
    pub text: String,
    pub dot: Option<String>,
    /// Action id to invoke when this row is clicked.
    pub action: Option<String>,
    /// Opaque per-row payload handed to that action as `LUVUS_MODULE_ROW_VALUE`
    /// — what turns a list of branches into a list of *buttons* (docs/13 §3.10).
    pub value: Option<String>,
    /// Extra actions offered when this row is **right-clicked** (docs/52).
    ///
    /// Empty — the default, and what every pre-existing module pushes — means the
    /// row has no context menu, exactly as before. luvus cannot infer a menu for
    /// a row it does not understand (unlike a FILES row, which is a path), so the
    /// module declares one per row: a device row can offer "flash this board"
    /// while a command row in the same dock offers nothing.
    pub menu: Vec<DockRowMenuItem>,
}

/// One entry in a dock row's right-click menu (docs/52). `destructive` only
/// tints the label — luvus does not confirm on the module's behalf.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DockRowMenuItem {
    pub title: String,
    /// Action id to invoke. Empty means a non-interactive divider.
    pub action: String,
    /// Payload for *this item*, overriding the row's `value` when present.
    ///
    /// The row's `value` says which thing the row is (a port, a branch); an
    /// item's says which variant of the action to run. Without it every entry
    /// in a menu would need its own action id — the same row-explosion the
    /// row-level `value` exists to prevent, one level down.
    pub value: Option<String>,
    pub destructive: bool,
}

impl DockRowMenuItem {
    pub fn is_divider(&self) -> bool {
        self.action.is_empty()
    }
}

/// A right-click context menu on a module dock row (docs/52).
///
/// Everything needed to run a click is **snapshotted here when the menu opens**,
/// never re-read from `module_docks` afterwards. Module dock rows are the most
/// volatile thing luvus renders — any `ui.dock.push` replaces the whole list, and
/// pollers do that on a timer — so indexing back into the live rows at click time
/// could run a *different* row's action. With `erase flash` on one of them, that
/// is a destructive mistake, not a cosmetic one.
pub struct DockMenu {
    pub dock_id: String,
    pub row_index: usize,
    /// Top-left corner of the popup (the click point, clamped to fit on screen).
    pub anchor: (u16, u16),
    /// The row's menu, snapshotted at open time.
    pub items: Vec<DockRowMenuItem>,
    /// The row's identity at open time, replayed into the action's env.
    pub row_text: String,
    pub row_value: Option<String>,
    /// Module that owns the dock, resolved at open time.
    pub owner: Option<String>,
    /// One clickable rect per item, filled in by the renderer.
    pub rects: Vec<Rect>,
}

/// A module-contributed dock's cached content (title + rows). luvus owns the
/// rendering; the module only pushes data via `ui.dock.push`.
#[derive(Clone)]
pub struct ModuleDock {
    pub title: String,
    pub rows: Vec<DockRow>,
}

/// One sidebar's live state: shown/hidden, width, and its ordered docks.
#[derive(Clone)]
pub struct SideState {
    pub visible: bool,
    pub width: u16,
    pub docks: Vec<DockKind>,
}

impl SideState {
    fn from_config(c: &crate::config::SideConfig) -> SideState {
        let mut docks: Vec<DockKind> = c.docks.iter().map(|s| DockKind::from_id(s)).collect();
        // Enforce the per-side cap on load: a hand-edited or pre-cap config with
        // more than `MAX_DOCKS_PER_SIDE` here keeps the first few; the overflow
        // falls to "off" (unmounted, still in the registry to re-place).
        docks.truncate(MAX_DOCKS_PER_SIDE);
        SideState {
            visible: c.visible,
            width: c.width.clamp(SIDEBAR_WIDTH_MIN, SIDEBAR_WIDTH_MAX),
            docks,
        }
    }
    fn to_config(&self) -> crate::config::SideConfig {
        crate::config::SideConfig {
            visible: self.visible,
            width: self.width,
            docks: self.docks.iter().map(|d| d.id().to_string()).collect(),
        }
    }
    /// True if this sidebar should occupy screen space (shown and non-empty).
    pub fn shown(&self) -> bool {
        self.visible && !self.docks.is_empty()
    }
    /// True if `kind` is mounted in this sidebar.
    pub fn has(&self, kind: &DockKind) -> bool {
        self.docks.contains(kind)
    }
}

/// The left + right sidebars and their docks (docs/29).
#[derive(Clone)]
pub struct Sidebars {
    pub left: SideState,
    pub right: SideState,
}

impl Sidebars {
    pub fn get(&self, side: Side) -> &SideState {
        match side {
            Side::Left => &self.left,
            Side::Right => &self.right,
        }
    }
    pub fn get_mut(&mut self, side: Side) -> &mut SideState {
        match side {
            Side::Left => &mut self.left,
            Side::Right => &mut self.right,
        }
    }
    fn from_config(cfg: &crate::config::SidebarsConfig) -> Sidebars {
        Sidebars {
            left: SideState::from_config(&cfg.left),
            right: SideState::from_config(&cfg.right),
        }
    }
    fn to_config(&self) -> crate::config::SidebarsConfig {
        crate::config::SidebarsConfig {
            left: self.left.to_config(),
            right: self.right.to_config(),
        }
    }
    /// Whether `side` has a free dock slot (below `MAX_DOCKS_PER_SIDE`, docs/29).
    pub fn has_room(&self, side: Side) -> bool {
        self.get(side).docks.len() < MAX_DOCKS_PER_SIDE
    }
    /// Which side, if any, currently holds `kind`.
    pub fn side_of(&self, kind: &DockKind) -> Option<Side> {
        if self.left.has(kind) {
            Some(Side::Left)
        } else if self.right.has(kind) {
            Some(Side::Right)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Normal,
    Prefix,
    /// Keyboard pane-resize mode (docs/27, RESIZE-3): arrows/`hjkl` resize the
    /// focused pane; `Esc`/`Enter`/`q` leave. Entered via `Ctrl+Space r`.
    Resize,
}

pub struct Tab {
    /// Stable public identity. Positions remain the human-facing CLI locator;
    /// API integrations use this value across moves, swaps, and restarts.
    pub id: String,
    pub layout: TileLayout,
    /// When `Some`, this is a **git tab** (docs/17): render the git dashboard
    /// instead of panes. The `layout` holds a placeholder leaf (no real pane is
    /// spawned), so all existing `layout()` code keeps working unchanged.
    pub git: Option<Box<crate::git::GitView>>,
    /// When `true`, this is the **orchestration board** (docs/22, ORCH-7): render
    /// the task/lease dashboard from `App.orch` instead of panes. Same placeholder
    /// -leaf trick as a git tab; mutually exclusive with `git`.
    pub orch: bool,
    /// When `true`, this is the **Mission Control** dashboard (docs/54): the
    /// workspace or all-workspaces agent overview. Same placeholder-leaf trick
    /// as git/orch; mutually exclusive with both.
    pub mission: bool,
    /// User-chosen tab name (docs/28). `None` → the tab bar shows its number.
    /// Git/orch/mission tabs keep their fixed label and are never named.
    pub name: Option<String>,
}

impl Tab {
    /// A normal pane tab.
    fn panes(layout: TileLayout) -> Tab {
        Tab {
            id: crate::ids::public_id("tab"),
            layout,
            git: None,
            orch: false,
            mission: false,
            name: None,
        }
    }

    pub fn is_git(&self) -> bool {
        self.git.is_some()
    }

    pub fn is_orch(&self) -> bool {
        self.orch
    }

    pub fn is_mission(&self) -> bool {
        self.mission
    }

    /// Pane tabs can be renamed; the git/orch/mission dashboards keep their label.
    pub fn is_renameable(&self) -> bool {
        !self.is_git() && !self.is_orch() && !self.is_mission()
    }
}

/// The "what's running here?" overlay for one pane (click its title bar).
///
/// An agent's own UI elides long commands (`Bash(cargo test …)`) and those
/// characters never reach luvus, so the *screen* can't be expanded. The OS still
/// knows the real argv, and luvus owns the pane's child pid — so this reads the
/// process tree instead, and shows the command in full.
pub struct CmdInspect {
    pub pane: PaneId,
    pub cwd: PathBuf,
    /// Snapshot taken when the overlay opened (and on `r`), never per frame.
    pub procs: Vec<crate::platform::ProcInfo>,
    pub scroll: usize,
}

/// The tab-rename modal (docs/28): the tab being renamed + its editable buffer,
/// pre-filled with the current name. Opened from a pane tab's context menu.
pub struct TabRename {
    pub target: TabMenuTarget,
    pub buffer: String,
}

/// Stable-enough identity for a tab context-menu target. A tab's complete leaf
/// set is unique inside a live session, including dashboard placeholder leaves.
/// Resolving this snapshot at click time prevents an intervening API reorder
/// from making the menu act on whichever tab later occupies the old index.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TabMenuTarget {
    pub workspace: usize,
    pub leaves: Vec<PaneId>,
}

/// A right-click context menu on a tab. Reorder availability and module actions
/// are snapshotted when it opens, while target identities are resolved against
/// the live tab order when the user clicks.
pub struct TabMenu {
    pub target: TabMenuTarget,
    pub anchor: (u16, u16),
    pub items: Vec<(TabMenuItem, Rect)>,
    pub module_actions: Vec<ModuleMenuAction>,
    pub can_rename: bool,
    pub can_move_left: bool,
    pub can_move_right: bool,
    /// Every other tab, snapshotted for the Swap With submenu.
    pub swap_targets: Vec<(TabMenuTarget, String)>,
    pub swap_open: bool,
    pub swap_rects: Vec<(TabMenuTarget, Rect)>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TabMenuItem {
    Rename,
    MoveLeft,
    MoveRight,
    SwapWith,
    Divider,
    /// The `i`-th module action declaring `contexts = ["tab"]`.
    Module(usize),
}

/// Cap a custom tab name so a pathological paste can't bloat the session.
pub(crate) const TAB_NAME_MAX: usize = 40;

/// A right-click context menu on a WORKSPACES row: rename / worktree / close the
/// node. Opened by right-clicking a workspace in the sidebar.
pub struct WsMenu {
    /// Stable target identity. Workspace indices shift when another workspace
    /// is closed through the API while this menu is open.
    pub workspace_id: String,
    /// Whether the target was a Git repository when the menu opened. Repository
    /// detection launches `git`, so snapshot it once instead of doing process
    /// I/O on every frame while the popup is visible.
    pub is_repo: bool,
    /// Top-left corner of the popup (the click point, clamped to fit on screen).
    pub anchor: (u16, u16),
    /// Each visible item + its clickable rect, filled in by the renderer.
    pub items: Vec<(WsMenuItem, Rect)>,
    /// Module actions offered here, snapshotted when the menu opened (docs/13
    /// §3.8) so a registry change mid-menu can't shift what a click runs.
    pub module_actions: Vec<ModuleMenuAction>,
}

/// An action offered by the workspace context menu. Worktree / git actions only
/// appear for nodes inside a git repo. `Divider` is a non-interactive separator.
/// `Module(i)` is the `i`-th module action declaring `contexts = ["workspace"]`
/// (docs/13 §3.8), resolved against the live registry when clicked.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum WsMenuItem {
    Pin,
    Unpin,
    Close,
    Rename,
    /// Delete a **linked worktree** and its files (git worktree remove + folder).
    /// Only offered for worktree nodes, never a main checkout / plain workspace.
    DeleteWorktree,
    NewWorktree,
    OpenWorktree,
    Divider,
    OpenGit,
    OpenOrch,
    /// Open the Mission Control dashboard for this node (docs/54).
    OpenMission,
    Module(usize),
}

/// At or below this many columns the UI uses the automatic mobile presentation
/// (docs/100). The threshold is inclusive and configurable per installation.
pub const MOBILE_WIDTH: u16 = 64;

/// A destination in the touch **switcher** overlay (docs/18): jump to a pane,
/// switch nodes, or make a new one. Big tap targets for a phone.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SwitcherTarget {
    Pane(PaneId),
    /// A tab, addressed by its workspace + tab index (docs/65 — the window list).
    Tab {
        ws: usize,
        tab: usize,
    },
    Workspace(usize),
    NewWorkspace,
    NewTab,
    Settings,
    MissionControl,
    Version,
    Exit,
}

/// One rendered row of the switcher — a section header or a tappable item.
pub enum SwitcherRow {
    Header(String),
    Agent {
        target: SwitcherTarget,
        state: State,
        title: String,
        location: String,
    },
    /// A tab row (docs/65): a jump to a specific tab in a workspace.
    Tab {
        target: SwitcherTarget,
        name: String,
        location: String,
        active: bool,
    },
    Node {
        target: SwitcherTarget,
        name: String,
        branch: Option<String>,
        active: bool,
    },
    Action {
        target: SwitcherTarget,
        label: String,
        detail: String,
    },
}

/// Which sections the switcher shows (docs/65). `All` lists everything; the
/// others narrow to one category so `w` (window list) and `s` (session tree) can
/// open the switcher pre-scoped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SwitcherScope {
    All,
    Agents,
    Tabs,
    Workspaces,
}

impl SwitcherScope {
    /// The chip order (also the `Tab`-to-cycle order).
    pub const ALL: [SwitcherScope; 4] = [
        SwitcherScope::All,
        SwitcherScope::Agents,
        SwitcherScope::Tabs,
        SwitcherScope::Workspaces,
    ];

    /// True if this scope shows `section` (one of Agents/Tabs/Workspaces).
    pub fn shows(self, section: SwitcherScope) -> bool {
        self == SwitcherScope::All || self == section
    }

    /// The next scope in the chip order (wraps), for `Tab`.
    pub fn next(self) -> SwitcherScope {
        let i = Self::ALL.iter().position(|s| *s == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    /// Localized chip label.
    pub fn label(self, cat: &crate::i18n::Catalog) -> &'static str {
        match self {
            SwitcherScope::All => cat.switch_scope_all,
            SwitcherScope::Agents => cat.agents,
            SwitcherScope::Tabs => cat.switch_scope_tabs,
            SwitcherScope::Workspaces => cat.workspaces,
        }
    }
}

/// A right-click context menu on a FILES-dock row (docs/38 FILE-6): file/folder
/// CRUD. Snapshots the target path when opened so a tree change mid-menu can't
/// retarget it.
pub struct FileMenu {
    pub path: PathBuf,
    pub is_dir: bool,
    pub anchor: (u16, u16),
    pub items: Vec<(FileMenuItem, Rect)>,
    /// Editors offered for this file (snapshot of `App.editors` when the menu
    /// opened), so `OpenWith(i)` resolves stably even if the cache changes. Empty
    /// for a folder (open actions are file-only).
    pub editors: Vec<(String, String)>,
}

/// A right-click menu for one exact DIFF list entry. The key is snapshotted so
/// an asynchronous status refresh cannot retarget the user's action.
pub struct DiffMenu {
    pub key: crate::diff::DiffKey,
    pub anchor: (u16, u16),
    pub items: Vec<(DiffMenuItem, Rect)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DiffMenuItem {
    OpenPreview,
    OpenPane,
    OpenTab,
    CopyPath,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FileMenuItem {
    /// Open in the native read-only viewer (files only).
    OpenReadonly,
    /// Open with editor `editors[i]` (files only).
    OpenWith(usize),
    NewFile,
    NewFolder,
    Rename,
    CopyPath,
    Divider,
    Delete,
}

impl FileMenu {
    /// This menu's rows, in order. A file leads with the open actions (read-only
    /// + one per detected editor) and a divider; a folder skips straight to CRUD.
    pub fn build_items(&self) -> Vec<FileMenuItem> {
        let mut v = Vec::new();
        if !self.is_dir {
            v.push(FileMenuItem::OpenReadonly);
            v.extend((0..self.editors.len()).map(FileMenuItem::OpenWith));
            v.push(FileMenuItem::Divider);
        }
        v.extend([
            FileMenuItem::NewFile,
            FileMenuItem::NewFolder,
            FileMenuItem::Rename,
            FileMenuItem::CopyPath,
            FileMenuItem::Divider,
            FileMenuItem::Delete,
        ]);
        v
    }
}

/// A text-input modal for creating/renaming a file-tree entry (docs/38 FILE-6).
pub struct FilePrompt {
    pub kind: FilePromptKind,
    /// The directory the new entry lands in (New*) or the rename's parent.
    pub dir: PathBuf,
    /// The path being renamed (Rename only).
    pub target: Option<PathBuf>,
    pub buffer: String,
    /// A failed create/rename shows its reason here and keeps the prompt open.
    pub error: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FilePromptKind {
    NewFile,
    NewFolder,
    Rename,
}

/// Cap a file-tree name entry (same spirit as [`TAB_NAME_MAX`]).
pub(crate) const FILE_NAME_MAX: usize = 120;

/// A right-click context menu **inside a pane**: split or close it. Opened by
/// right-clicking anywhere in a pane's area.
pub struct PaneMenu {
    /// The right-clicked pane the actions target.
    pub pane: PaneId,
    /// Top-left corner of the popup (the click point, clamped on-screen).
    pub anchor: (u16, u16),
    /// Each visible item + its clickable rect, filled in by the renderer.
    pub items: Vec<(PaneMenuItem, Rect)>,
    /// Module actions offered here, snapshotted when the menu opened (docs/13 §3.8).
    pub module_actions: Vec<ModuleMenuAction>,
    /// "Move to tab" destinations (target + label), snapshotted at open. The
    /// submenu lists these when the "Move to tab" row is hovered.
    pub move_targets: Vec<(MoveTarget, String)>,
    /// Whether the move-to-tab submenu is showing (sticky: set while hovering the
    /// "Move to tab" row or the submenu itself, so the border gap doesn't flicker
    /// it off). Driven by the renderer from `App.hover`.
    pub move_open: bool,
    /// Submenu row rects (target + rect), filled by the renderer for hit-testing.
    pub tab_rects: Vec<(MoveTarget, Rect)>,
    /// What was under the right-click, if anything (docs/58). Snapshotted with the
    /// rest of the menu because the pane keeps printing while it is open, so
    /// re-reading the grid on click could open a different target than the one the
    /// row was drawn for.
    pub link: Option<LinkTarget>,
    /// Whether this pane runs a fork-capable agent, snapshotted at open. Gates
    /// whether the "Fork to new pane" row is shown (docs/23).
    pub can_fork: bool,
}

/// Where "Move to tab" sends the pane: an existing tab (by index) or a fresh one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MoveTarget {
    Tab(usize),
    NewTab,
}

/// Why an existing pane could not be re-parented to another tab. Indices in
/// [`MoveTarget`] are internal (zero-based); the CLI/API translates its public
/// one-based tab numbers before calling the mutation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PaneMoveError {
    PaneNotFound,
    SourceNotPaneTab,
    TargetOutOfRange,
    SameTab,
    TargetNotPaneTab,
    NoChange,
}

/// The pane's new location after a successful move (internal zero-based indices).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PaneMoveResult {
    pub workspace: usize,
    pub tab: usize,
}

/// Why a tab could not be moved to a new position.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TabMoveError {
    PositionOutOfRange,
    SamePosition,
    AlreadyFirst,
    AlreadyLast,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TabMoveDirection {
    Left,
    Right,
}

/// Why an explicit tab focus request could not be applied.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TabFocusError {
    PositionOutOfRange,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TabRenameError {
    PositionOutOfRange,
    Dashboard,
    NameTooLong,
}

/// Why an existing agent session could not be forked into a sibling pane.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AgentForkError {
    PaneNotFound,
    SourceNotPaneTab,
    UnsupportedAgent,
    SessionUnknown,
    SpawnFailed,
}

/// The live pane and location created by an agent-session fork.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AgentForkResult {
    pub from: PaneId,
    pub pane: PaneId,
    pub agent: String,
    pub workspace: usize,
    pub tab: usize,
}

/// An action offered by the pane context menu. `SplitVertical` puts the new pane
/// side by side (a vertical divider, like `v`); `SplitHorizontal` stacks it (a
/// horizontal divider, like `s`). `Divider` is a non-interactive separator.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PaneMenuItem {
    SplitVertical,
    SplitHorizontal,
    /// "Open Link" — open the URL that was under the right-click (docs/58). Only
    /// offered when there actually was one, so the row is never a dead end.
    OpenLink,
    /// "Open File" — open the file path that was under the right-click, in luvus
    /// rather than the OS (docs/58). Offered only for a path that exists.
    OpenFile,
    /// "Fork to new pane" — branch this pane's agent session into a new pane to
    /// the right, preserving the original's context (docs/23). Shown only for
    /// fork-capable agents.
    ForkPane,
    /// "What's running here?" — the OS process tree for this pane (docs/07).
    RunningCmd,
    /// Parent row: hovering it opens a submenu of tabs to move this pane into.
    MoveToTab,
    /// "Rename" this pane (sets its live name, shown on the title strip).
    RenamePane,
    Divider,
    Close,
    /// The `i`-th module action declaring `contexts = ["pane"]` (docs/13 §3.8).
    Module(usize),
}

impl PaneMenuItem {
    /// The built-in rows, in render order. Module actions are appended after a
    /// divider by [`App::pane_menu_items`].
    pub const ALL: &'static [PaneMenuItem] = &[
        PaneMenuItem::SplitVertical,
        PaneMenuItem::SplitHorizontal,
        PaneMenuItem::ForkPane,
        PaneMenuItem::OpenLink,
        PaneMenuItem::OpenFile,
        PaneMenuItem::RunningCmd,
        PaneMenuItem::RenamePane,
        PaneMenuItem::MoveToTab,
        PaneMenuItem::Divider,
        PaneMenuItem::Close,
    ];
}

/// What an [`AgentMenu`] targets: a resumable on-disk session (by list index) or
/// a live agent pane.
#[derive(Clone, Copy)]
pub enum AgentTarget {
    Session(usize),
    Live(PaneId),
}

/// A right-click context menu on an AGENTS-list row. A resumable session offers
/// **Resume** (reopen) + **Close** (remove from the list); a live agent offers
/// **Close** (close its pane).
pub struct AgentMenu {
    pub target: AgentTarget,
    pub anchor: (u16, u16),
    pub items: Vec<(AgentMenuItem, Rect)>,
    /// Module actions offered here, snapshotted when the menu opened (docs/13 §3.8).
    pub module_actions: Vec<ModuleMenuAction>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AgentMenuItem {
    Resume,
    /// "Rename" a live agent's pane (sets its live name). Live agents only.
    RenamePane,
    Close,
    /// Pin a live agent to the top of the AGENTS list (per-session).
    Pin,
    Unpin,
    Divider,
    /// The `i`-th module action declaring `contexts = ["agent"]` (docs/13 §3.8).
    Module(usize),
}

impl AgentMenu {
    /// The built-in items for a given target, in render order.
    pub fn items_for(target: AgentTarget) -> Vec<AgentMenuItem> {
        match target {
            AgentTarget::Session(_) => vec![AgentMenuItem::Resume, AgentMenuItem::Close],
            AgentTarget::Live(_) => vec![AgentMenuItem::RenamePane, AgentMenuItem::Close],
        }
    }
}

/// The workspace-rename modal: like [`TabRename`] but for a node's **label** (the
/// folder on disk is never touched). Pre-filled with the current name.
pub struct WsRename {
    /// Stable target identity. Workspace indices can shift while the modal is
    /// open if another workspace closes through the API.
    pub workspace_id: String,
    pub buffer: String,
}

/// Cap a custom workspace name (same reasoning as [`TAB_NAME_MAX`]). Shared
/// with the CLI so local validation and the socket mutation agree.
pub(crate) const WS_NAME_MAX: usize = 40;

/// Why workspace metadata could not be changed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WorkspaceUpdateError {
    NotFound,
    EmptyName,
    NameTooLong,
}

/// The pane-rename modal: sets a pane's live name (the same alias `pane name` /
/// `agent name` set), which addresses the agent and shows on the pane's title.
/// Pre-filled with the current name. The grammar is enforced as you type.
pub struct PaneRename {
    pub pane: PaneId,
    pub buffer: String,
}

/// Cap a live pane name at the addressable-name length (`[a-z][a-z0-9_-]{0,31}`).
const PANE_NAME_MAX: usize = 32;

/// The in-TUI **new-task form** (ORCH-7): create an orchestration task without the
/// CLI. Fields are plain text; `paths`/`deps` are whitespace-split on submit.
#[derive(Default)]
pub struct OrchForm {
    pub title: String,
    pub paths: String,
    pub deps: String,
    pub gate: String,
    /// Active field: 0=title · 1=paths · 2=deps · 3=gate.
    pub field: usize,
    pub error: Option<String>,
}

impl OrchForm {
    pub const FIELDS: usize = 4;

    /// The currently-edited field's text.
    pub fn active_mut(&mut self) -> &mut String {
        match self.field {
            0 => &mut self.title,
            1 => &mut self.paths,
            2 => &mut self.deps,
            _ => &mut self.gate,
        }
    }

    /// The four fields' current values, in order, for rendering.
    pub fn values(&self) -> [&String; 4] {
        [&self.title, &self.paths, &self.deps, &self.gate]
    }
}

/// A forwarded mouse press held by a mouse-tracking pane app (see
/// `App.mouse_grab`): the pressed button (with modifier bits already encoded)
/// plus the app's drag/SGR flags captured at press time.
#[derive(Clone, Copy)]
pub struct MouseGrab {
    pub pane: PaneId,
    pub btn: u16,
    pub drag: bool,
    pub sgr: bool,
}

/// The board's **start-worker picker**: choose which agent to launch in the
/// task's isolated worktree (or a plain shell). Opened by `s` on the board.
pub struct OrchStart {
    /// The task a worker is being started for.
    pub task: String,
    /// Selected row in [`crate::app::board::agent_choices`].
    pub cursor: usize,
}

pub struct Workspace {
    /// Stable public identity across display reordering and restarts.
    pub id: String,
    pub name: String,
    pub cwd: PathBuf,
    /// Current git branch of `cwd`, if it's inside a repo (for the WORKSPACES list).
    pub branch: Option<String>,
    /// Ahead/behind upstream, set when this workspace's git tab fetches status (docs/17).
    pub git_ahead_behind: Option<(u32, u32)>,
    /// Worktree grouping (docs/18 WT): present for any workspace inside a git repo;
    /// workspaces sharing a `common_dir` are checkouts of one repo and group together.
    pub worktree: Option<crate::git::WorktreeMembership>,
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
    /// Pinned to the top of the WORKSPACES list (right-click → Pin). Persisted.
    pub pinned: bool,
}

/// A native agent session reported by an integration hook (M6), used to resume
/// the agent after a restart (e.g. `claude --resume <id>`).
#[derive(Clone)]
pub struct AgentSession {
    pub agent: String,
    pub session_id: String,
}

/// Explicit agent lifecycle authority supplied by an integration. It is
/// intentionally ephemeral and bounded by a lease: a crashed adapter cannot
/// leave a pane permanently stuck in a fabricated state.
#[derive(Clone)]
pub struct AgentReport {
    pub source: String,
    pub agent: String,
    pub state: State,
    pub message: Option<String>,
    pub sequence: u64,
    pub expires_at: Instant,
}

/// Per-pane detection state (the runtime side of agent awareness).
pub struct PaneStatus {
    pub state: State,
    pub agent: String,
    pub last_activity: Instant,
    /// When the user last sent input (keystrokes/paste) to this pane. Lets
    /// detection tell a user typing (whose echo is also output) apart from the
    /// agent generating (docs/07). Defaults old so unfocused/new panes aren't
    /// gated.
    pub last_input: Instant,
    pub seen: bool,
    pub agent_session: Option<AgentSession>,
    /// Consecutive successful process scans that saw this pane's shell/process
    /// tree but not the agent bound in `agent_session`. Two confirmations mean
    /// the agent really returned to the shell, rather than briefly disappearing
    /// during startup/re-exec; see `apply_proc_scan`.
    agent_absent_scans: u8,
    prev_working: bool,
    done: bool,
    /// Whether a blocked/done bell may fire. Set false after one fires; re-armed
    /// only when the pane is focused (seen). Stops a bursty/streaming agent —
    /// which flaps Working↔Idle↔Done — from ringing the bell on every pause.
    notify_armed: bool,
    /// The state the raw classifier currently *wants*, awaiting the debounce
    /// dwell before it becomes the published `state`. Together with
    /// `candidate_since` this is the hysteresis gate (see `QUIET_DWELL`).
    candidate: State,
    candidate_since: Instant,
    /// When this pane's PTY was last resized. A resize (e.g. switching to a tab
    /// whose panes have a different geometry) makes the agent repaint its whole
    /// screen; during that reflow-then-repaint the grid is transiently unreliable,
    /// so detection is frozen for `RESIZE_GRACE` afterward — otherwise a reflowed
    /// old spinner line flips an idle agent to "working" for the ~2.5s the Idle
    /// dwell then takes to clear. `None` until the pane is first resized (docs/07).
    pub last_resize: Option<Instant>,
    /// Last terminal-output generation whose title and bottom rows were read.
    last_detect_generation: Option<u64>,
    /// Cached terminal inputs for classification. Unchanged panes reuse these
    /// while still evaluating activity and quiet-dwell deadlines.
    detected_title: Option<Arc<str>>,
    detected_bottom: Arc<str>,
    /// Process identity, resize, manifest, and session changes force one fresh
    /// terminal inspection even when no new PTY bytes arrived.
    pub(crate) force_detect: bool,
    /// For a Blocked agent, the on-screen line it is waiting on (its bottom-text
    /// snippet), so Mission Control can show *why* it's blocked and offer a
    /// one-key answer. Captured **once** when the pane enters Blocked (not every
    /// tick), cleared when it leaves; `None` when the pane isn't blocked.
    pub blocked_hint: Option<String>,
    /// Explainable evidence from the last heuristic classification.
    pub identity_source: &'static str,
    pub state_source: &'static str,
    pub rule_priority: Option<i32>,
    pub rule_region: Option<&'static str>,
    /// Optional authoritative state lease from an agent integration.
    pub agent_report: Option<AgentReport>,
}

impl PaneStatus {
    fn new(agent: String) -> Self {
        PaneStatus {
            state: State::Idle,
            agent,
            last_activity: Instant::now(),
            // Old by default so a freshly spawned pane's first output isn't gated
            // as "the user is typing".
            last_input: Instant::now()
                .checked_sub(Duration::from_secs(3600))
                .unwrap_or_else(Instant::now),
            seen: true,
            agent_session: None,
            agent_absent_scans: 0,
            prev_working: false,
            done: false,
            notify_armed: true,
            candidate: State::Idle,
            candidate_since: Instant::now(),
            last_resize: None,
            last_detect_generation: None,
            detected_title: None,
            detected_bottom: Arc::from(""),
            force_detect: true,
            blocked_hint: None,
            identity_source: "command_fallback",
            state_source: "no_positive_state_evidence",
            rule_priority: None,
            rule_region: None,
            agent_report: None,
        }
    }
}

/// What a `Ctrl`+click will actually do (docs/58) — resolved, so a target only
/// exists if it is real: a URL that passed the scheme whitelist, or a path that
/// was found on disk relative to the pane's working directory.
#[derive(Debug, Clone, PartialEq)]
pub enum LinkTarget {
    /// Hand to the client's browser.
    Url(String),
    /// Open in luvus's own viewer or editor, exactly like a FILES click (docs/38),
    /// jumping to `line` when the reference carried one.
    File { path: PathBuf, line: Option<u32> },
}

/// The link currently under the mouse, whose grid it belongs to, and what it
/// resolved to (docs/58).
///
/// `link.spans` are **grid** coordinates, the same space
/// `VtEngine::for_each_cell` reports, so the renderer tests a cell directly with
/// no arithmetic per frame.
#[derive(Debug, Clone, PartialEq)]
pub struct HoverLink {
    pub pane: PaneId,
    pub link: crate::links::Link,
    pub target: LinkTarget,
}

/// A `Ctrl`+press that landed on a link, held until its release.
///
/// The same gesture dragged is the RESIZE-5 divider grab, so the two are told
/// apart by movement: leave the press cell and the resize takes over, release on
/// it and the link opens.
#[derive(Debug, Clone)]
pub struct LinkPress {
    pub target: LinkTarget,
    /// Screen cell of the press.
    pub at: (u16, u16),
}

/// A drag text-selection inside a pane. Screen coordinates keep native file
/// views aligned with their renderer. Terminal panes additionally store stable
/// retained-history coordinates, so scrolling the viewport cannot move either
/// endpoint onto different text.
#[derive(Clone, Copy)]
pub struct Selection {
    pub pane: PaneId,
    pub content: Rect,
    pub anchor: (u16, u16),
    pub cursor: (u16, u16),
    pub retained: Option<RetainedSelection>,
    /// The active gesture deliberately moved the retained endpoint by wheel.
    /// Incoming PTY output may also remap retained coordinates, but must never
    /// turn a stationary click into a copied range.
    pub scrolled: bool,
    pub dragging: bool,
}

/// Mouse-selection endpoints in the same absolute retained-row coordinate
/// space used by keyboard copy mode (oldest retained row is zero).
#[derive(Clone, Copy)]
pub struct RetainedSelection {
    pub anchor: (usize, usize),
    pub cursor: (usize, usize),
}

impl RetainedSelection {
    pub(crate) fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    pub(crate) fn contains(&self, row: usize, col: usize, width: usize) -> bool {
        let ((sr, sc), (er, ec)) = self.ordered();
        if row < sr || row > er {
            return false;
        }
        let middle_left = sc.min(ec);
        let left = if row == sr { sc } else { middle_left };
        let right = if row == er {
            ec
        } else {
            width.saturating_sub(1)
        };
        col >= left && col <= right
    }
}

/// An in-progress pane-divider resize drag (docs/27, RESIZE-2): the split node
/// being dragged, addressed by its path in the layout tree.
pub struct ResizeDrag {
    pub path: Vec<bool>,
    pub axis: Axis,
}

/// Cells of slack around a divider that still count as grabbing it. The gap
/// between panes puts the two visible border lines ~2 cells apart, so a ±2 zone
/// makes the seam comfortably grabbable without stealing clicks from content.
const RESIZE_GRAB_TOL: u16 = 2;

/// How many columns onto the content side of a sidebar's edge still grab it for a
/// resize (docs/29). Widens the 1-column seam into a comfortable target without
/// reaching into the sidebar body (where dock rows own the width).
const SIDEBAR_GRAB_TOL: u16 = 2;

impl Selection {
    /// (start, end) terminal cells in reading order (top-left → bottom-right).
    pub(crate) fn ordered(&self) -> ((u16, u16), (u16, u16)) {
        let key = |p: (u16, u16)| (p.1, p.0);
        if key(self.anchor) <= key(self.cursor) {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    /// Whether terminal cell `(x, y)` is inside the selection (and the pane's
    /// content area) — drives the render highlight.
    pub fn contains(&self, x: u16, y: u16) -> bool {
        let c = self.content;
        if x < c.x || x >= c.right() || y < c.y || y >= c.bottom() {
            return false;
        }
        let ((sx, sy), (ex, ey)) = self.ordered();
        if y < sy || y > ey {
            return false;
        }
        // Middle rows keep the drag's left edge instead of expanding into the
        // pane margin. This keeps the highlighted range and copied text aligned.
        let left = if y == sy { sx } else { sx.min(ex) };
        let right = if y == ey {
            ex
        } else {
            c.right().saturating_sub(1)
        };
        x >= left && x <= right
    }

    /// True only when the drag actually moved (so a plain click isn't a copy).
    fn has_range(&self) -> bool {
        self.anchor != self.cursor
            || (self.scrolled
                && self
                    .retained
                    .is_some_and(|selection| selection.anchor != selection.cursor))
    }
}

/// Keyboard-driven selection in a terminal pane. Rows are absolute indices in
/// retained-row coordinates (oldest retained row is zero), so the selection remains
/// stable while its viewport scrolls.
#[derive(Clone, Copy)]
pub struct CopyMode {
    pub pane: PaneId,
    pub anchor: (usize, usize),
    pub cursor: (usize, usize),
    /// The viewport to restore when the user cancels instead of copying.
    pub saved_scroll: usize,
}

impl CopyMode {
    pub(crate) fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    pub(crate) fn contains(&self, row: usize, col: usize) -> bool {
        let ((sr, sc), (er, ec)) = self.ordered();
        if row < sr || row > er {
            return false;
        }
        let left = if row == sr { sc } else { 0 };
        let right = if row == er { ec } else { usize::MAX };
        col >= left && col <= right
    }
}

pub struct App {
    pub panes: HashMap<PaneId, Pane>,
    /// One random value for this server lifetime. Harness runtimes from an old
    /// server fail closed even if a process-local pane route is later reused.
    pub(crate) backend_server_generation: String,
    /// O(1) stable terminal identity → mutable pane route lookup. Pending
    /// deferred panes are absent until `PtyReady` reaches the app loop.
    pub(crate) backend_terminal_index: HashMap<String, PaneId>,
    /// Harness-assigned display labels are separate from addressable agent
    /// aliases and from child-controlled OSC titles.
    pub(crate) backend_labels: HashMap<PaneId, String>,
    /// Bounded, event-driven protocol waits keyed by pane. These observe the
    /// PTY's monotonic content revision and never poll from a socket worker.
    pub(crate) backend_revision_waits:
        HashMap<PaneId, Vec<crate::app::backend::BackendRevisionWait>>,
    pub(crate) last_backend_wait_scan: Instant,
    pub status: HashMap<PaneId, PaneStatus>,
    /// Live agent aliases: a human name → the pane whose agent it points at, set
    /// via `agent.name` so `agent.send` / `agent.keys` / `agent.read` can address
    /// an agent by name instead of a pane id. Ephemeral (pane ids are reallocated
    /// each run), so it is never persisted and is pruned when a pane closes.
    pub agent_names: HashMap<String, PaneId>,
    /// Agent-detection rule set: built-ins plus user `~/.luvus/manifests/*.toml`
    /// (docs/07). Loaded once at startup.
    pub manifests: crate::detect::Manifests,
    /// Terminal editors found on `PATH` (+ `$EDITOR`), for "open file with"
    /// (docs/38). `(run command, label)`. Probed once at startup, off the render
    /// path — see `platform::editor_choices`.
    pub editors: Vec<(String, String)>,
    pub workspaces: Vec<Workspace>,
    pub active_ws: usize,
    pub theme: Theme,
    /// Built-in, installed, and virtual themes in Settings display order.
    /// Loaded from the shared home-level `themes/` directory; rendering reads
    /// this in-memory snapshot and never touches the filesystem.
    pub theme_registry: crate::theme::ThemeRegistry,
    /// Active UI-language catalog (docs/21), resolved from `config.language`.
    pub catalog: &'static crate::i18n::Catalog,
    /// Persisted user configuration (theme, layout, notifications, keys).
    pub config: crate::config::Config,
    /// Active `key → Cmd` map for prefix mode (defaults + config overrides).
    pub keymap: std::collections::HashMap<String, Cmd>,
    /// The parsed prefix chord (docs/64), from `config.prefix`. Default Ctrl+Space.
    pub prefix: keys::PrefixSpec,
    /// The open Settings modal, if any (`Some` ⇒ modal captures input).
    pub settings: Option<SettingsUi>,
    /// The open folder picker (workspace chooser), if any (captures input).
    pub picker: Option<FolderPicker>,
    /// Clickable targets in the open folder picker. Specific controls precede
    /// the modal body in hit-test order.
    pub picker_rects: Vec<(PickerHit, Rect)>,
    /// Whether the keyboard-shortcut cheat-sheet overlay is open (`Ctrl+Space ?`).
    pub help_open: bool,
    /// Whether the changelog modal is open (click the status-line version number).
    /// Shows every shipped release's notes; captures input while open.
    pub changelog_open: bool,
    /// Scroll offset (in lines) into the changelog modal body.
    pub changelog_scroll: u16,
    /// Set by the background update check (docs) to the newer version string
    /// (e.g. `"0.9.3"`) when a release is available; shown as a dot by the
    /// version number and a banner in the changelog modal. `None` = up to date.
    pub update_available: Option<String>,
    /// The "what is actually running in this pane?" overlay (docs/07): a
    /// snapshot of the pane's process tree, taken once when it opens. Click a
    /// pane's title to open it. `None` = closed.
    pub cmd_inspect: Option<CmdInspect>,
    /// Clickable pane-title strips, set by the renderer each frame.
    pub pane_title_rects: Vec<(PaneId, Rect)>,
    /// New-worktree branch-name prompt (docs/18 WT): `Some(buf)` ⇒ the modal is
    /// open, holding the branch being typed.
    pub worktree_prompt: Option<String>,
    /// Active tab-rename modal (docs/28); `None` when closed.
    pub tab_rename: Option<TabRename>,
    /// Active tab context menu; `None` when closed.
    pub tab_menu: Option<TabMenu>,
    /// The workspace right-click context menu, and the workspace-rename modal.
    pub ws_menu: Option<WsMenu>,
    /// Armed worktree-delete confirmation: the stable workspace identity of the
    /// worktree to delete once confirmed (`y`/⏎). Destructive — removes the
    /// folder + git worktree — so it goes through the confirm modal like the file
    /// delete. Resolve the current index only when confirmation is committed.
    pub worktree_delete: Option<String>,
    /// Active pane context menu (right-click inside a pane); `None` when closed.
    pub pane_menu: Option<PaneMenu>,
    /// Active AGENTS-list context menu (right-click a row); `None` when closed.
    pub agent_menu: Option<AgentMenu>,
    /// Live agents pinned to the top of the AGENTS list (right-click → Pin).
    /// Per-session: pane ids are reallocated each run, so this is not persisted;
    /// pruned when a pane closes.
    pub pinned_agents: std::collections::HashSet<PaneId>,
    pub ws_rename: Option<WsRename>,
    /// The pane-rename modal (`Some` ⇒ open, captures input).
    pub pane_rename: Option<PaneRename>,
    /// Clickable ⏎-commit / esc-cancel footer buttons of whichever text-input
    /// modal is open (worktree prompt / tab rename / workspace rename), set each
    /// render so the mouse layer can hit-test them.
    pub modal_commit_rect: Option<Rect>,
    pub modal_cancel_rect: Option<Rect>,
    /// The repo the pending worktree is created in — the active workspace's folder
    /// (`Ctrl+Space G`) or the folder browsed in the picker (`w`).
    pub worktree_repo: Option<PathBuf>,
    /// The last worktree-create error (e.g. branch already checked out), shown in
    /// the prompt so a failed create isn't silent. Cleared when the user edits.
    pub worktree_error: Option<String>,
    pub mode: Mode,
    /// Left + right sidebars, their widths, and their docks (docs/29). Resolved
    /// from `config.sidebars()` at startup; runtime edits persist via `save_sidebars`.
    pub sidebars: Sidebars,
    /// Module-contributed dock content, keyed by dock id (docs/29, DOCK-4).
    /// Populated by `ui.dock.push`; rendered by the sidebar.
    pub module_docks: std::collections::HashMap<String, ModuleDock>,
    /// Clickable rows of module docks this frame: (dock id, row index, rect).
    pub module_dock_rects: Vec<(String, usize, Rect)>,
    /// Server-owned Luvus Bar registry, notifications, and active-attachment
    /// hit geometry. Rendering representations remain viewport-local.
    pub bar: crate::bar::BarState,
    pub zoomed: bool,
    pub should_quit: bool,
    /// True when this `App` is owned by the background server. A server session
    /// outlives its windows: closing the last workspace resets to a fresh one
    /// instead of quitting — only `server stop` ends it. The single-process
    /// `--local` run leaves this false and quits like a normal terminal app.
    pub server_mode: bool,
    pub spinner: u64,
    /// Structure changed since the last save; the loop persists when set.
    pub session_dirty: bool,
    pub events: EventBus,
    /// Multi-agent orchestration ledger + path leases (docs/22, ORCH-1/2). Kept
    /// in its own file (`orch.json`), independent of the session snapshot.
    pub orch: crate::orch::OrchState,
    /// Scroll offset of the orchestration board tab (docs/22, ORCH-7).
    pub orch_scroll: usize,
    /// Selected task row on the board (for keyboard/mouse actions).
    pub orch_cursor: usize,
    /// The in-TUI new-task form, when open (ORCH-7).
    pub orch_form: Option<OrchForm>,
    /// The board's "start worker with…" agent picker, when open.
    pub orch_start: Option<OrchStart>,
    /// Task whose detail overlay is open on the board (`o`), plus its scroll.
    pub orch_detail: Option<String>,
    pub orch_detail_scroll: usize,
    /// Last agent chosen in the start picker — the next picker opens on it.
    pub orch_last_agent: usize,
    /// The board's content rect, for mouse-wheel hit-testing.
    pub orch_area: Rect,
    /// Mission Control (docs/54): scroll + selected row of the active mission tab,
    /// its content rect (mouse-wheel hit-testing), the rows currently displayed
    /// (so keyboard activation maps back to a pane or session), and the async
    /// token/cost cache.
    pub mission_scroll: usize,
    pub mission_cursor: usize,
    /// Current workspace only, or every open workspace.
    pub mission_scope: crate::mission::MissionScope,
    /// Click targets for the two scope tabs. Agent rows remain keyboard-only.
    pub mission_scope_rects: Vec<(crate::mission::MissionScope, Rect)>,
    /// Click target for the explicit Mission Control usage refresh action.
    pub mission_refresh_rect: Option<Rect>,
    pub mission_area: Rect,
    pub mission_rows: Vec<crate::mission::MissionRowView>,
    /// Row index whose Mission Control detail overlay is open (`o`), if any (MC-5).
    pub mission_detail: Option<usize>,
    /// The inline "answer the agent" input (docs/54): `Some(text)` while typing a
    /// reply to the selected blocked agent; `⏎` sends it to that pane, `esc` cancels.
    pub mission_answer: Option<String>,
    /// Fleet burn rate in USD/hour (docs/54), from the change in total cost between
    /// usage scans; `None` until two scans have landed.
    pub mission_burn: Option<f64>,
    /// Previous (total cost, time) sample, for the burn-rate delta.
    pub mission_last_cost: Option<(f64, std::time::Instant)>,
    /// Best-effort usage (tokens/context/cost) keyed by **agent + session id**, so
    /// a live pane and its resumable on-disk session share one entry without
    /// colliding with another agent's local session namespace. Refreshed
    /// off-loop and blitted by the mission render — never computed on the render
    /// path (docs/54 MC-2/MC-4).
    pub agent_usage:
        std::collections::HashMap<crate::mission::UsageKey, crate::mission::AgentUsage>,
    /// Each scanned transcript's mtime, so the next usage scan re-reads a session
    /// only when its file actually changed (docs/54) — an idle session costs one
    /// `stat`, not a full read+parse.
    pub usage_mtimes: std::collections::HashMap<crate::mission::UsageKey, std::time::SystemTime>,
    /// Cursor position from the last render (for headless frame streaming).
    pub last_cursor: Option<(u16, u16)>,
    /// Foreground client asked to detach (prefix+q). Distinct from quit.
    pub detach_requested: bool,
    /// The foreground client selected another named session in the global
    /// finder. The server consumes this once and sends a logical handoff only
    /// to that client.
    pub pending_session_switch: Option<String>,
    /// The last node was closed, ending the session (docs/43 §3.3). *Every*
    /// client detaches, so the window closes, while the server stays up with no
    /// nodes — distinct from `detach_requested` (one client leaves, session
    /// continues) and from `should_quit` (the server itself exits).
    pub end_session: bool,
    /// Force the next frame to be a **full** repaint (not a diff), so a terminal
    /// whose screen was damaged outside luvus's knowledge — a window move/resize,
    /// regaining focus, another program's output — repaints cleanly. The render
    /// loop consumes and clears it. Set on any resize and on focus-regained.
    pub force_redraw: bool,
    /// Notification messages queued by detection; the loop flushes them to the
    /// terminal (bell + desktop) and clears.
    pub pending_notify: Vec<String>,
    /// Set when an agent just finished (transition to Done); the loop plays the
    /// retro "done" jingle once and clears it.
    pub pending_sound: bool,
    /// Active mouse text selection in a pane (drag to select). Cleared on a new
    /// click; on release its text is queued to `pending_clipboard`.
    pub selection: Option<Selection>,
    /// Keyboard copy selection. This deliberately owns navigation keys so they
    /// cannot reach the child while text is being selected.
    pub copy_mode: Option<CopyMode>,
    /// A mouse button forwarded into a mouse-tracking pane app: set on press so
    /// the matching drag/release reach the same app even if the cursor leaves
    /// the pane mid-drag. Caches the app's drag/SGR flags from press time so
    /// drags and releases touch no engine lock (the PTY reader holds that mutex
    /// during output bursts).
    pub mouse_grab: Option<MouseGrab>,
    /// Text to copy to the client's system clipboard (via OSC 52) — set when a
    /// selection finishes, drained + broadcast by the loop.
    pub pending_clipboard: Option<String>,
    /// A URL to open in the client's browser (docs/58) — set by a Ctrl+click on a
    /// link in a pane, drained + broadcast by the loop like `pending_clipboard`.
    pub pending_open_url: Option<String>,
    /// The cell `hover_link` was resolved for, so holding `Ctrl` while resting on a
    /// cell does not rescan. Cleared when `Ctrl` is released, so pointing at a
    /// link *first* and pressing `Ctrl` after still lights it up.
    pub link_scan_at: Option<(u16, u16)>,
    /// The link under the mouse, recomputed only when the hovered cell changes so
    /// nothing scans the grid per frame. Rendered underlined by `ui/panes.rs`.
    pub hover_link: Option<HoverLink>,
    /// A `Ctrl`+press that landed on a link. Held until release, because the same
    /// gesture dragged is the RESIZE-5 divider grab: moving off the cell hands
    /// the press over to the resize, releasing on it opens the link.
    pub link_press: Option<LinkPress>,
    /// A transient toast (text, expiry) shown bottom-center — e.g. "Copied".
    pub toast: Option<(String, Instant)>,
    /// Downsample RGB → 256-color (for the local path on non-truecolor terms).
    pub downsample: bool,
    /// Throttle for refreshing pane working directories.
    last_cwd_at: Instant,
    /// Resumable agent sessions discovered on disk (for the AGENTS sidebar).
    pub resumable: Vec<crate::agent::SessionInfo>,
    /// A resumable-session disk scan is running on a worker thread; don't start
    /// another until its `SessionsScanned` result arrives.
    sessions_scan_inflight: bool,
    /// Command lines running in each pane, refreshed off-loop (docs/07). The
    /// authoritative answer to "which agent is this?", since an agent is a
    /// process, not a word on screen. Empty for a pane we could not scan.
    pub(crate) proc_commands: HashMap<PaneId, Vec<String>>,
    /// One process scan at a time, same guard as the session scan.
    proc_scan_inflight: bool,
    /// Session ids the user removed from the sidebar list (hidden, not deleted).
    pub dismissed_sessions: HashSet<String>,
    /// Throttle for rescanning the agents' on-disk session stores.
    last_sessions_at: Instant,
    last_proc_at: Instant,
    /// Mission Control usage is demand-driven: opening/focusing the dashboard,
    /// changing its scope, or choosing refresh queues one off-loop scan. No
    /// usage reader runs merely because a hidden Mission Control tab exists.
    mission_usage_requested: bool,
    /// Workspace whose Mission Control tab was visible on the previous sync.
    /// `None` also records transitions away from Mission Control.
    mission_active_workspace: Option<usize>,
    usage_scan_inflight: bool,
    /// Throttle for per-pane agent classification — it locks each pane's VT engine
    /// and scans its grid, so it runs at ~100ms, not at the render frame rate.
    last_detect_at: Instant,
    /// Panes whose PTY generation or identity inputs changed since their last
    /// classification pass. The detector consumes this bounded set instead of
    /// walking every quiet pane on each 100 ms cadence.
    detection_dirty: HashSet<PaneId>,
    /// A bounded fleet audit repairs any missed invalidation without making the
    /// audit the normal scheduling path.
    last_detection_audit_at: Instant,
    /// Runtime evidence for the generation gate, exposed additively by
    /// `pane.list` for diagnostics and performance comparisons.
    detection_extractions: u64,
    detection_skips: u64,
    detection_panes_considered: u64,
    detection_full_fleet_audits: u64,
    detection_audit_recoveries: u64,
    /// Pending server-side `wait.output` requests keyed by pane (docs/81).
    /// Satisfied by the pane's next output event, expired by the loop tick.
    output_waits: HashMap<PaneId, Vec<crate::app::dispatch::OutputWait>>,
    /// Event-driven semantic waits keyed by pane. Unlike the old CLI-side
    /// subscribe/poll composition, registration and the initial state check are
    /// atomic on the app loop, so a transition cannot fall through the gap.
    agent_waits: HashMap<PaneId, Vec<crate::app::dispatch::AgentWait>>,
    /// Atomic launch/readiness workflows keyed by their new or selected pane.
    agent_starts: HashMap<PaneId, crate::app::dispatch::AgentStart>,
    /// Submitted prompts waiting for post-submission state/output evidence.
    agent_prompts: HashMap<PaneId, Vec<crate::app::dispatch::AgentPrompt>>,
    /// Throttle for re-scanning parked waiters — the scan locks each waiting
    /// pane's VT engine and rebuilds its recent text, so it runs at ~100ms,
    /// not at the render frame rate. Deadline expiry still runs every tick.
    last_output_wait_scan: Instant,
    /// Scroll offsets + scrollable regions for the two sidebar lists, so long
    /// WORKSPACES / AGENTS lists can be wheeled through.
    pub workspaces_scroll: usize,
    pub agents_scroll: usize,
    pub workspaces_area: Rect,
    pub agents_area: Rect,
    /// The FILES dock (docs/38): the tree model, its scroll region, and the
    /// clickable rect per visible row (`(row index, rect)`), re-set each frame.
    pub file_tree: crate::files::FileTree,
    /// `files.tree` callers waiting for the off-loop root directory read.
    /// The targeted root prevents a workspace switch from redirecting a reply.
    pending_file_tree_api: Vec<(PathBuf, crate::ipc::api::ApiRequest)>,
    pub files_area: Rect,
    pub file_tree_rects: Vec<(usize, Rect)>,
    /// FILES/DIFF header controls and DIFF list rows (docs/88).
    pub files_mode: crate::diff::FilesMode,
    pub files_mode_rects: Vec<(crate::diff::FilesMode, Rect)>,
    pub diff_row_rects: Vec<(usize, Rect)>,
    /// Visible source rows inside native DIFF panes. A click selects the exact
    /// stack-row identity and old/new side before note actions run.
    pub diff_source_rects: Vec<(PaneId, usize, crate::diff::DiffSide, Rect)>,
    /// Visible saved-note cards inside native DIFF panes. A left click opens
    /// that exact note in the inline editor.
    pub diff_note_rects: Vec<(PaneId, String, Rect)>,
    /// Pane currently owning a mouse drag that selects an annotation range.
    pub diff_note_drag: Option<PaneId>,
    pub diff: crate::diff::DiffState,
    pub diff_agent_picker: Option<crate::diff::DiffAgentPicker>,
    pub diff_menu: Option<DiffMenu>,
    /// API requests waiting for the shared off-loop FILES/DIFF status scan.
    /// Each retains the workspace root it targeted so a later workspace switch
    /// cannot redirect a parked mutation or read into a different repository.
    pending_diff_api: Vec<(PathBuf, crate::ipc::api::ApiRequest)>,
    /// Working-tree git status per path, for tinting the FILES dock (docs/38).
    /// Refreshed off-loop; empty when the tree root isn't a repo.
    pub file_git_status: HashMap<PathBuf, crate::git::local::FileStatus>,
    git_status_inflight: bool,
    last_git_status_at: Instant,
    /// When the FILES tree last re-read its on-screen directories, to catch files
    /// created or deleted outside luvus (docs/38). Gated so a huge repo isn't
    /// re-scanned every tick.
    last_file_scan_at: Instant,
    /// FILES-dock right-click menu + its modals (docs/38 FILE-6).
    pub file_menu: Option<FileMenu>,
    /// Module-dock row right-click menu (docs/52).
    pub dock_menu: Option<DockMenu>,
    pub file_prompt: Option<FilePrompt>,
    /// The path a delete-confirm modal is asking about.
    pub file_delete: Option<PathBuf>,
    /// Compact (touch) mode is active — recomputed each frame from the width
    /// (docs/18). Drives the single-pane layout and the `≡` switcher button.
    pub compact: bool,
    /// The touch **switcher** overlay is open (docs/18).
    pub switcher: bool,
    /// Keyboard cursor into the switcher's tappable rows.
    pub switcher_cursor: usize,
    /// Scroll offset (in item rows) so the switcher works with more
    /// agents/nodes than fit on a phone screen.
    pub switcher_scroll: usize,
    /// Type-to-filter query for the switcher palette (docs/65). Empty = no filter.
    pub switcher_query: String,
    /// Which section(s) the switcher lists (docs/65).
    pub switcher_scope: SwitcherScope,
    /// Each switcher row's target + clickable rect, set by the renderer.
    pub switcher_rects: Vec<(SwitcherTarget, Rect)>,
    /// The scope chips' rects (docs/65), set by the renderer for click-to-switch.
    pub switcher_scope_rects: Vec<(SwitcherScope, Rect)>,
    /// The mobile MENU button's rect, for tap hit-testing.
    pub switcher_button_rect: Option<Rect>,
    /// Previous/next pane halves of the mobile header's second row. They are
    /// present only when the active tab contains more than one pane.
    pub mobile_pane_prev_rect: Option<Rect>,
    pub mobile_pane_next_rect: Option<Rect>,
    /// The mobile navigator's explicit CLOSE button. Desktop switcher palettes
    /// do not set this geometry.
    pub switcher_close_rect: Option<Rect>,
    /// The global scrollback-search overlay (docs/63). `Some` => it owns input.
    pub search: Option<GlobalSearch>,
    /// A brief highlight of the line a search jump landed on (docs/63).
    pub search_flash: Option<SearchFlash>,
    /// Native **view panes** (docs/38 FILE-3): a leaf id maps to a non-PTY
    /// renderer here instead of a `Pane` in `panes`. Invariant: a leaf is in
    /// `panes` **xor** `views`.
    pub views: HashMap<PaneId, ViewKind>,
    /// Panes running a terminal **editor** on a file (docs/38): the pane is a
    /// real PTY (so it is in `panes`, not `views`), but the tab bar labels it
    /// with the file exactly like a read-only view tab. Deliberately not
    /// persisted — after a restart the pane is no longer that editor, so the
    /// label must not survive it. Untracked in `drop_leaf_runtime`.
    pub editor_files: HashMap<PaneId, PathBuf>,
    /// Most recently opened files, newest first, scoped by workspace folder.
    /// This is a small in-memory finder convenience and is never persisted.
    pub recent_files: VecDeque<(PathBuf, PathBuf)>,
    /// Reused single-click **preview** panes. Each workspace may own one so
    /// browsing FILES or DIFF never focuses and replaces another workspace's
    /// preview.
    pub preview_views: HashSet<PaneId>,
    /// AGENTS list filter: `true` shows only live (active) agents; `false`
    /// (the default) also shows the resumable session history.
    pub agents_active_only: bool,
    /// Last active workspace shown, to auto-reveal it on a programmatic change.
    pub last_active_ws_shown: usize,
    /// Last mouse position, for hover affordances (the session delete ✕).
    pub hover: Option<(u16, u16)>,
    app_tx: Sender<AppEvent>,
    pub last_pane_area: Rect,
    // Hit-test geometry from the last render, for mouse clicks.
    pub pane_rects: Vec<(PaneId, Rect)>,
    /// Each pane's **content** rect (inside the border/title) — maps a mouse
    /// position to a grid cell for text selection.
    pub pane_content_rects: Vec<(PaneId, Rect)>,
    /// When `Some`, keyboard **scroll mode** is active on this pane: plain keys
    /// scroll its scrollback (see `handle_scroll_mode_key`) instead of reaching
    /// the agent. Entered by wheel-up or `Shift+↑`; left by `q`/typing. A
    /// Mac-friendly path that needs no `Ctrl+Space` prefix.
    pub scroll_pane: Option<PaneId>,
    /// Active pane-divider resize drag (docs/27, RESIZE-2); `None` when idle.
    pub resize_drag: Option<ResizeDrag>,
    /// Divider under the cursor, for the hover highlight (RESIZE-4).
    pub hover_divider: Option<crate::layout::Divider>,
    /// Active sidebar-edge resize drag (docs/29) — which side is being dragged;
    /// `None` when idle. Width updates live during the drag and is persisted once
    /// on release, so no `config.json` write lands on the per-event path.
    pub sidebar_resize: Option<Side>,
    /// Sidebar whose edge seam is under the cursor, for the hover highlight.
    pub hover_sidebar: Option<Side>,
    /// The draggable edge seam (`│` column) of each shown sidebar, set every frame
    /// in `render`; `None` when that sidebar is hidden. Hit-tested by
    /// `sidebar_seam_at`.
    pub left_seam: Option<Rect>,
    pub right_seam: Option<Rect>,
    /// The full content area (frame minus the status bar), stored so an in-flight
    /// sidebar drag can turn a cursor column into a width off the correct edge.
    pub last_main_area: Rect,
    pub tab_rects: Vec<(usize, Rect)>,
    pub tab_close_rects: Vec<(usize, Rect)>,
    pub ws_rects: Vec<(usize, Rect)>,
    /// Clickable git-branch text per workspace (opens the git tab — docs/17).
    pub workspace_branch_rects: Vec<(usize, Rect)>,
    /// Clickable view-selector tabs in the active git tab (Commits/Flow/…).
    pub git_section_rects: Vec<(crate::git::Section, Rect)>,
    /// The All/Active filter toggle in the AGENTS header (`bool` = active_only).
    pub agents_filter_rects: Vec<(bool, Rect)>,
    pub agent_rects: Vec<(PaneId, Rect)>,
    /// Resumable-session rows in the sidebar (index into `resumable`).
    pub session_rects: Vec<(usize, Rect)>,
    /// The ✕ delete buttons on hovered resumable rows (index into `resumable`).
    pub new_ws_rect: Option<Rect>,
    /// Tab-bar scroll arrows (when tabs overflow), for mouse hit-testing.
    pub tab_prev_rect: Option<Rect>,
    pub tab_next_rect: Option<Rect>,
    /// The focused pane's ✕ close button, for mouse hit-testing.
    pub pane_close_rect: Option<Rect>,
    /// The focused pane's ⤢ zoom/restore button (docs/18): a touch-reachable
    /// equivalent of `Ctrl+Space z`, so a split can be expanded to fullscreen on
    /// a phone where there is no keyboard. Only shown when the pane is wide
    /// enough to hold it beside the ✕.
    pub pane_zoom_rect: Option<Rect>,
    /// The left sidebar's collapse/reopen toggle button, for mouse hit-testing.
    pub sidebar_toggle_rect: Option<Rect>,
    /// The right sidebar's collapse/reopen toggle button (docs/29).
    pub right_sidebar_toggle_rect: Option<Rect>,
    /// The bottom-right status-line version, clickable to open the changelog modal.
    pub version_rect: Option<Rect>,
    /// Bounds of the changelog popup. Used to distinguish harmless clicks in
    /// its content from clicks on the dimmed backdrop that dismiss it.
    pub changelog_modal_rect: Option<Rect>,
    /// The changelog modal's close button, for mouse hit-testing.
    pub changelog_close_rect: Option<Rect>,
    /// The changelog modal's "check for updates" button. `None` when the modal is
    /// shut, and also when the title row is too narrow to hold it.
    pub changelog_check_rect: Option<Rect>,
    /// Clickable links on the changelog modal's **visible** rows: commit and PR
    /// refs from the notes, plus the "read it all on luvus.dev" row at the end.
    /// Rebuilt each frame from the rows actually on screen, so scrolling a link
    /// out of view takes its click target with it.
    pub changelog_link_rects: Vec<(Rect, String)>,
    /// Copyable installer/update command rows at the top of the changelog modal.
    /// Each hit stores the exact untruncated command written to the clipboard.
    pub changelog_copy_rects: Vec<(Rect, String)>,
    /// Cached display rows for the changelog modal, keyed by `(body width, theme
    /// name)`. Flattening the notes into wrapped, styled rows allocates, and the
    /// modal redraws every frame — so it is built once per open and reused until
    /// the modal is resized or the theme changes under it.
    pub changelog_rows: Option<(u16, String, Vec<crate::ui::changelog::Row>)>,
    // Settings modal hit-test geometry (populated by render when the modal is open).
    pub settings_icon_rect: Option<Rect>,
    pub settings_close_rect: Option<Rect>,
    pub settings_modal_rect: Option<Rect>,
    pub settings_tab_rects: Vec<(SettingsTab, Rect)>,
    pub settings_ctl_rects: Vec<(usize, Rect)>,
    /// Right-aligned remove actions for installed theme rows. Store the stable
    /// theme ID rather than its registry index so a reload cannot retarget a click.
    pub settings_theme_remove_rects: Vec<(String, Rect)>,
    /// Installed themes with an off-loop uninstall worker in flight. An optional
    /// `(previous theme, selection revision)` restores an automatically replaced
    /// active theme only when the user has not selected another theme meanwhile.
    pub(crate) pending_theme_uninstalls: HashMap<String, Option<(String, u64)>>,
    pub(crate) theme_selection_revision: u64,
    /// Slider arrows in the modal: (control index, ±1 direction, rect).
    pub settings_arrow_rects: Vec<(usize, i32, Rect)>,
    /// Installed modules (docs/13) and the ring buffer of their command logs.
    pub modules: crate::module::ModuleRegistry,
    pub module_logs: Vec<crate::module::ModuleCommandLog>,
    /// Live module panes by pane id, untracked automatically on close (MOD-2).
    pub module_panes: HashMap<PaneId, crate::module::ModulePaneRecord>,
    /// Modules whose `[[startup]]` hooks have already run this process, so a
    /// re-entrant call (link, enable, socket-ready) can't run them twice.
    pub module_startup_done: std::collections::HashSet<String>,
    /// The module-settings row being edited in Settings → Modules, if any.
    pub module_setting_edit: Option<ModuleSettingEdit>,
}

/// The inline text prompt for a `type = "string"` module setting (docs/13 §3.6).
/// Number/bool/enum settings are stepped with the `‹ ›` arrows instead.
pub struct ModuleSettingEdit {
    pub module_id: String,
    pub key: String,
    pub title: String,
    pub buffer: String,
    pub secret: bool,
}

impl App {
    pub fn new(cols: u16, rows: u16, app_tx: Sender<AppEvent>) -> Result<App> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let name = ws_name(&cwd);

        let config = crate::config::load();
        let files_show_hidden = config.layout.files_show_hidden;
        crate::layout::set_gaps(config.layout.col_gap, config.layout.row_gap);
        let theme_registry = crate::theme::ThemeRegistry::load();
        let theme = theme_registry.theme_or_default(&config.theme);
        let catalog = crate::i18n::by_code(&config.language);
        let sidebars = Sidebars::from_config(&config.sidebars());
        let shell = crate::platform::resolve_shell(&config.shell);
        let keymap = keys::build_keymap(&config.keybindings);
        let prefix = keys::PrefixSpec::parse(&config.prefix).unwrap_or_default();
        let modules = crate::module::registry::load();
        let mut bar = crate::bar::BarState::default();
        bar.sync_modules(&modules);

        let id = PaneId::alloc();
        let pane = Pane::spawn(
            id,
            cols,
            rows,
            cwd.clone(),
            app_tx.clone(),
            None,
            &shell,
            config.scrollback_bytes(),
        )?;
        let command = pane.command.clone();
        let mut panes = HashMap::new();
        panes.insert(id, pane);
        let backend_server_generation =
            crate::terminal::backend::random_id().map_err(anyhow::Error::msg)?;
        let backend_terminal_index = panes
            .iter()
            .filter_map(|(pane_id, pane)| {
                pane.terminal_runtime()
                    .map(|runtime| (runtime.terminal_id, *pane_id))
            })
            .collect();
        let mut status = HashMap::new();
        status.insert(id, PaneStatus::new(command));

        let mut app = App {
            panes,
            backend_server_generation,
            backend_terminal_index,
            backend_labels: HashMap::new(),
            backend_revision_waits: HashMap::new(),
            last_backend_wait_scan: Instant::now(),
            status,
            manifests: crate::detect::Manifests::load(&crate::persist::ensure_manifests_dir()),
            editors: crate::platform::editor_choices(),
            workspaces: vec![Workspace {
                id: crate::ids::public_id("workspace"),
                name,
                worktree: worktree_membership(&cwd),
                cwd,
                branch: None,
                git_ahead_behind: None,
                pinned: false,
                tabs: vec![Tab::panes(TileLayout::new(id))],
                active_tab: 0,
            }],
            active_ws: 0,
            theme,
            theme_registry,
            catalog,
            config,
            keymap,
            prefix,
            agent_names: HashMap::new(),
            settings: None,
            picker: None,
            picker_rects: Vec::new(),
            help_open: false,
            changelog_open: false,
            changelog_scroll: 0,
            update_available: None,
            cmd_inspect: None,
            pane_title_rects: Vec::new(),
            worktree_prompt: None,
            tab_rename: None,
            tab_menu: None,
            ws_menu: None,
            worktree_delete: None,
            pane_menu: None,
            agent_menu: None,
            pinned_agents: std::collections::HashSet::new(),
            ws_rename: None,
            pane_rename: None,
            modal_commit_rect: None,
            modal_cancel_rect: None,
            worktree_repo: None,
            worktree_error: None,
            mode: Mode::Normal,
            sidebars,
            module_docks: std::collections::HashMap::new(),
            module_dock_rects: Vec::new(),
            bar,
            zoomed: false,
            should_quit: false,
            server_mode: false,
            spinner: 0,
            session_dirty: true,
            events: api::new_bus(),
            orch: crate::orch::OrchState::load(),
            orch_scroll: 0,
            orch_cursor: 0,
            orch_form: None,
            orch_start: None,
            orch_detail: None,
            orch_detail_scroll: 0,
            orch_last_agent: 0,
            orch_area: Rect::ZERO,
            mission_scroll: 0,
            mission_cursor: 0,
            mission_scope: crate::mission::MissionScope::Workspace,
            mission_scope_rects: Vec::new(),
            mission_refresh_rect: None,
            mission_area: Rect::ZERO,
            mission_rows: Vec::new(),
            mission_detail: None,
            mission_answer: None,
            mission_burn: None,
            mission_last_cost: None,
            agent_usage: std::collections::HashMap::new(),
            usage_mtimes: std::collections::HashMap::new(),
            last_cursor: None,
            detach_requested: false,
            pending_session_switch: None,
            end_session: false,
            force_redraw: false,
            pending_notify: Vec::new(),
            pending_sound: false,
            selection: None,
            copy_mode: None,
            mouse_grab: None,
            pending_clipboard: None,
            pending_open_url: None,
            link_scan_at: None,
            hover_link: None,
            link_press: None,
            toast: None,
            downsample: false,
            last_cwd_at: Instant::now(),
            resumable: Vec::new(),
            sessions_scan_inflight: false,
            proc_commands: HashMap::new(),
            proc_scan_inflight: false,
            dismissed_sessions: HashSet::new(),
            last_sessions_at: Instant::now(),
            mission_usage_requested: false,
            mission_active_workspace: None,
            usage_scan_inflight: false,
            last_proc_at: Instant::now(),
            last_detect_at: Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(Instant::now),
            detection_dirty: HashSet::new(),
            last_detection_audit_at: Instant::now()
                .checked_sub(Duration::from_secs(2))
                .unwrap_or_else(Instant::now),
            detection_extractions: 0,
            detection_skips: 0,
            detection_panes_considered: 0,
            detection_full_fleet_audits: 0,
            detection_audit_recoveries: 0,
            output_waits: HashMap::new(),
            agent_waits: HashMap::new(),
            agent_starts: HashMap::new(),
            agent_prompts: HashMap::new(),
            last_output_wait_scan: Instant::now(),
            workspaces_scroll: 0,
            agents_scroll: 0,
            agents_active_only: false,
            workspaces_area: Rect::ZERO,
            agents_area: Rect::ZERO,
            // Rooted at nothing; the first detect tick re-roots it to the active
            // node (set_root is a no-op when already correct).
            file_tree: {
                // Start from the persisted show-hidden choice (docs/38). Read
                // from a hoisted local, since `config` is moved into its own
                // field above before this initializer runs.
                let mut t = crate::files::FileTree::new(std::path::PathBuf::new());
                t.show_hidden = files_show_hidden;
                t
            },
            pending_file_tree_api: Vec::new(),
            files_area: Rect::ZERO,
            file_tree_rects: Vec::new(),
            files_mode: crate::diff::FilesMode::Files,
            files_mode_rects: Vec::new(),
            diff_row_rects: Vec::new(),
            diff_source_rects: Vec::new(),
            diff_note_rects: Vec::new(),
            diff_note_drag: None,
            diff: crate::diff::DiffState::default(),
            diff_agent_picker: None,
            diff_menu: None,
            pending_diff_api: Vec::new(),
            views: HashMap::new(),
            editor_files: HashMap::new(),
            recent_files: VecDeque::new(),
            preview_views: HashSet::new(),
            file_git_status: HashMap::new(),
            git_status_inflight: false,
            last_git_status_at: Instant::now()
                .checked_sub(Duration::from_secs(10))
                .unwrap_or_else(Instant::now),
            last_file_scan_at: Instant::now()
                .checked_sub(Duration::from_secs(10))
                .unwrap_or_else(Instant::now),
            file_menu: None,
            dock_menu: None,
            file_prompt: None,
            file_delete: None,
            compact: false,
            switcher: false,
            search: None,
            search_flash: None,
            switcher_cursor: 0,
            switcher_scroll: 0,
            switcher_query: String::new(),
            switcher_scope: SwitcherScope::All,
            switcher_rects: Vec::new(),
            switcher_scope_rects: Vec::new(),
            switcher_button_rect: None,
            mobile_pane_prev_rect: None,
            mobile_pane_next_rect: None,
            switcher_close_rect: None,
            last_active_ws_shown: 0,
            hover: None,
            app_tx,
            last_pane_area: Rect::ZERO,
            pane_rects: Vec::new(),
            pane_content_rects: Vec::new(),
            scroll_pane: None,
            resize_drag: None,
            hover_divider: None,
            sidebar_resize: None,
            hover_sidebar: None,
            left_seam: None,
            right_seam: None,
            last_main_area: Rect::ZERO,
            tab_rects: Vec::new(),
            ws_rects: Vec::new(),
            workspace_branch_rects: Vec::new(),
            git_section_rects: Vec::new(),
            agents_filter_rects: Vec::new(),
            agent_rects: Vec::new(),
            session_rects: Vec::new(),
            tab_close_rects: Vec::new(),
            new_ws_rect: None,
            tab_prev_rect: None,
            tab_next_rect: None,
            pane_close_rect: None,
            pane_zoom_rect: None,
            sidebar_toggle_rect: None,
            right_sidebar_toggle_rect: None,
            version_rect: None,
            changelog_modal_rect: None,
            changelog_close_rect: None,
            changelog_check_rect: None,
            changelog_link_rects: Vec::new(),
            changelog_copy_rects: Vec::new(),
            changelog_rows: None,
            settings_icon_rect: None,
            settings_close_rect: None,
            settings_modal_rect: None,
            settings_tab_rects: Vec::new(),
            settings_ctl_rects: Vec::new(),
            settings_theme_remove_rects: Vec::new(),
            pending_theme_uninstalls: HashMap::new(),
            theme_selection_revision: 0,
            settings_arrow_rects: Vec::new(),
            modules,
            module_logs: Vec::new(),
            module_panes: HashMap::new(),
            module_startup_done: std::collections::HashSet::new(),
            module_setting_edit: None,
        };
        // A fresh start still loads `orch.json` — its pane bindings belong to a
        // previous server run, so rebind/clear them (same as `from_snapshot`).
        app.orch_reconcile();
        app.refresh_core_bar_widgets();
        Ok(app)
    }

    /// Restore the saved session, or start fresh if there is none / it fails.
    pub fn restore_or_new(cols: u16, rows: u16, app_tx: Sender<AppEvent>) -> Result<App> {
        if let Some(snap) = persist::load() {
            if let Some(mut app) = App::from_snapshot(snap, app_tx.clone()) {
                // Kick off the async fetch for any restored git tabs.
                app.refetch_git_tabs();
                return Ok(app);
            }
        }
        App::new(cols, rows, app_tx)
    }

    fn from_snapshot(snap: SessionSnapshot, app_tx: Sender<AppEvent>) -> Option<App> {
        let config = crate::config::load();
        let files_show_hidden = config.layout.files_show_hidden;
        let keymap = keys::build_keymap(&config.keybindings);
        let prefix = keys::PrefixSpec::parse(&config.prefix).unwrap_or_default();
        let shell = crate::platform::resolve_shell(&config.shell);
        let history_budget_bytes = config.scrollback_bytes();
        let modules = crate::module::registry::load();
        let mut panes = HashMap::new();
        let mut status = HashMap::new();
        let mut module_panes: HashMap<PaneId, crate::module::ModulePaneRecord> = HashMap::new();
        let mut views: HashMap<PaneId, ViewKind> = HashMap::new();
        let mut restored_names: Vec<(String, PaneId)> = Vec::new();
        let mut workspaces = Vec::new();
        for ws in snap.workspaces {
            let mut tabs = Vec::new();
            for tab in ws.tabs {
                // A git tab (docs/17): re-create the dashboard (no real panes) if
                // the folder is still a repo; it's re-fetched after the app is
                // built. If the folder is no longer a repo, the tab is dropped.
                if tab.git {
                    if crate::git::local::is_repo(&ws.cwd) {
                        let view = crate::git::GitView::new(ws.cwd.clone());
                        let placeholder = PaneId::alloc();
                        tabs.push(Tab {
                            id: tab.id.clone(),
                            layout: TileLayout::new(placeholder),
                            git: Some(Box::new(view)),
                            orch: false,
                            mission: false,
                            name: None,
                        });
                    }
                    continue;
                }
                // An orchestration board (docs/22): re-create the placeholder tab;
                // its data lives in this session's `orch.json` ledger, loaded already.
                if tab.orch {
                    let placeholder = PaneId::alloc();
                    tabs.push(Tab {
                        id: tab.id.clone(),
                        layout: TileLayout::new(placeholder),
                        git: None,
                        orch: true,
                        mission: false,
                        name: None,
                    });
                    continue;
                }
                // A Mission Control dashboard (docs/54): re-create the placeholder
                // tab; its agent/usage data is re-derived, nothing was stored.
                if tab.mission {
                    let placeholder = PaneId::alloc();
                    tabs.push(Tab {
                        id: tab.id.clone(),
                        layout: TileLayout::new(placeholder),
                        git: None,
                        orch: false,
                        mission: true,
                        name: None,
                    });
                    continue;
                }
                let mut remap = HashMap::new();
                for (raw, ps) in &tab.panes {
                    let id = PaneId::alloc();
                    // Re-attach the pane's live name to its new id (docs: names are
                    // pane-keyed and pane ids are reallocated each run).
                    if let Some(nm) = &ps.name {
                        restored_names.push((nm.clone(), id));
                    }
                    // A file-view leaf (docs/38 FILE-3): rebuild the view and
                    // re-read the file off-loop; no PTY is spawned.
                    if let Some(path) = &ps.file {
                        views.insert(
                            id,
                            ViewKind::File(crate::files::FileView::new(path.clone())),
                        );
                        let tx = app_tx.clone();
                        let p = path.clone();
                        std::thread::spawn(move || {
                            let load = crate::files::read_file(&p);
                            let _ = tx.send(crate::event::AppEvent::FileRead { id, load });
                        });
                        remap.insert(*raw, id);
                        continue;
                    }
                    // A DIFF leaf restores only its specification and display
                    // state. Current patch content is fetched again off-loop.
                    if let Some(spec) = &ps.diff {
                        let context_lines = spec.context_lines.min(crate::diff::MAX_CONTEXT_LINES);
                        let mut view = crate::diff::DiffView::new(
                            spec.root.clone(),
                            spec.key.clone(),
                            spec.preference,
                            context_lines,
                            spec.show_line_numbers,
                            spec.wrap,
                        );
                        view.request_token = 1;
                        view.scroll = spec.scroll;
                        view.selected = spec.selected;
                        view.selected_side = spec.selected_side;
                        view.horizontal = spec.horizontal;
                        views.insert(id, ViewKind::Diff(Box::new(view)));
                        let tx = app_tx.clone();
                        let root = spec.root.clone();
                        let file = crate::diff::DiffFile {
                            key: spec.key.clone(),
                            status: spec.status,
                            additions: None,
                            deletions: None,
                            binary: false,
                            unresolved_notes: 0,
                            viewed_fingerprint: None,
                            fingerprint: String::new(),
                        };
                        let context = context_lines;
                        std::thread::spawn(move || {
                            let result =
                                crate::diff::git::load_diff(&root, &file, context).map(|diff| {
                                    crate::diff::LoadedDiff {
                                        diff,
                                        reconciled_notes: Vec::new(),
                                    }
                                });
                            let _ = tx.send(crate::event::AppEvent::DiffLoaded {
                                id,
                                token: 1,
                                result,
                            });
                        });
                        remap.insert(*raw, id);
                        continue;
                    }
                    // Resume the native agent session captured at save time (a
                    // precise hook report, or one discovered from the agent's
                    // on-disk store keyed by cwd — see `persist::snapshot`).
                    // PowerShell can start directly on the resume command.
                    // POSIX and unrecognised shells start normally and receive
                    // it through the PTY after interactive profile setup.
                    // Re-apply the launch flags captured at save time (docs/62),
                    // unless Settings → General turns that off.
                    let resume = ps.agent_session.as_ref().and_then(|(agent, sid)| {
                        crate::agent::resume_for(
                            agent,
                            sid,
                            ps.agent_launch.as_deref(),
                            config.resume_launch_flags,
                        )
                    });
                    let resume_argv = resume.as_deref().and_then(|r| {
                        crate::platform::shell_run_then_interactive(&shell, r.trim())
                    });
                    // A module pane re-runs its entrypoint if the module is still
                    // installed + runnable; otherwise it falls back to a shell.
                    let restored = ps.module.as_ref().and_then(|(mid, ep)| {
                        restore_module_pane(&modules, mid, ep, id, &app_tx, history_budget_bytes)
                    });
                    let (pane, module_rec) = match restored {
                        Some((p, rec)) => (p, Some(rec)),
                        None => {
                            // Resolve a usable cwd before handing the shell to
                            // the deferred worker. Session loading must not wait
                            // for macOS/Linux/Windows PTY allocation, which can
                            // occasionally stall inside the OS. The saved screen
                            // is still replayed synchronously for the first frame.
                            let home = crate::platform::home_dir().unwrap_or_default();
                            let mut cwd_candidates = Vec::new();
                            for candidate in [&ps.cwd, &ws.cwd, &home] {
                                if candidate.is_dir() && !cwd_candidates.contains(candidate) {
                                    cwd_candidates.push(candidate.clone());
                                }
                            }
                            let Some((cwd, fallback_cwds)) = cwd_candidates.split_first() else {
                                continue;
                            };
                            let pane = match &resume_argv {
                                Some(argv) => Pane::spawn_shell_with_deferred(
                                    id,
                                    80,
                                    24,
                                    cwd.clone(),
                                    fallback_cwds,
                                    app_tx.clone(),
                                    ps.screen.as_deref(),
                                    &shell,
                                    argv,
                                    history_budget_bytes,
                                )
                                .ok(),
                                None => Some(Pane::spawn_restored(
                                    id,
                                    80,
                                    24,
                                    cwd.clone(),
                                    fallback_cwds,
                                    app_tx.clone(),
                                    ps.screen.as_deref(),
                                    &shell,
                                    history_budget_bytes,
                                )),
                            };
                            let Some(pane) = pane else {
                                continue;
                            };
                            (pane, None)
                        }
                    };
                    let direct_resume = resume_argv.is_some() && module_rec.is_none();
                    if let Some(rec) = module_rec {
                        module_panes.insert(id, rec);
                    }
                    let cmd = pane.command.clone();
                    let mut st = PaneStatus::new(cmd);
                    if let Some((agent, sid)) = &ps.agent_session {
                        st.agent = agent.clone();
                        st.agent_session = Some(AgentSession {
                            agent: agent.clone(),
                            session_id: sid.clone(),
                        });
                        if !direct_resume {
                            if let Some(r) = &resume {
                                pane.send(r.as_bytes());
                            }
                        }
                    }
                    panes.insert(id, pane);
                    status.insert(id, st);
                    remap.insert(*raw, id);
                }
                // A tree that references panes that failed to restore (or is
                // corrupt) drops only THIS tab — its surviving panes are
                // cleaned up and every other tab/workspace is kept, instead of
                // discarding the user's entire session.
                match TileLayout::from_tree(&tab.tree, &remap, tab.focus) {
                    Some(layout) => {
                        let mut t = Tab::panes(layout);
                        t.id = tab.id.clone();
                        t.name = tab.name.clone();
                        tabs.push(t);
                    }
                    None => {
                        for id in remap.values() {
                            panes.remove(id);
                            status.remove(id);
                            module_panes.remove(id);
                        }
                    }
                }
            }
            if tabs.is_empty() {
                continue;
            }
            let active_tab = ws.active_tab.min(tabs.len() - 1);
            workspaces.push(Workspace {
                id: ws.id,
                name: ws.name,
                worktree: worktree_membership(&ws.cwd),
                cwd: ws.cwd,
                branch: None,
                git_ahead_behind: None,
                pinned: ws.pinned,
                tabs,
                active_tab,
            });
        }
        if workspaces.is_empty() {
            return None;
        }
        let active_ws = snap.active_ws.min(workspaces.len() - 1);
        let backend_server_generation = crate::terminal::backend::random_id().ok()?;
        let backend_terminal_index = panes
            .iter()
            .filter_map(|(pane_id, pane)| {
                pane.terminal_runtime()
                    .map(|runtime| (runtime.terminal_id, *pane_id))
            })
            .collect();

        crate::layout::set_gaps(config.layout.col_gap, config.layout.row_gap);
        let theme_registry = crate::theme::ThemeRegistry::load();
        let theme = theme_registry.theme_or_default(&config.theme);
        let catalog = crate::i18n::by_code(&config.language);
        let sidebars = Sidebars::from_config(&config.sidebars());
        let mut bar = crate::bar::BarState::default();
        bar.sync_modules(&modules);
        // Restore live pane names, minus any whose pane failed to come back.
        let agent_names: HashMap<String, PaneId> = restored_names
            .into_iter()
            .filter(|(_, id)| panes.contains_key(id))
            .collect();

        let mut app = App {
            panes,
            backend_server_generation,
            backend_terminal_index,
            backend_labels: HashMap::new(),
            backend_revision_waits: HashMap::new(),
            last_backend_wait_scan: Instant::now(),
            status,
            manifests: crate::detect::Manifests::load(&crate::persist::ensure_manifests_dir()),
            editors: crate::platform::editor_choices(),
            workspaces,
            active_ws,
            theme,
            theme_registry,
            catalog,
            config,
            keymap,
            prefix,
            agent_names,
            settings: None,
            picker: None,
            picker_rects: Vec::new(),
            help_open: false,
            changelog_open: false,
            changelog_scroll: 0,
            update_available: None,
            cmd_inspect: None,
            pane_title_rects: Vec::new(),
            worktree_prompt: None,
            tab_rename: None,
            tab_menu: None,
            ws_menu: None,
            worktree_delete: None,
            pane_menu: None,
            agent_menu: None,
            pinned_agents: std::collections::HashSet::new(),
            ws_rename: None,
            pane_rename: None,
            modal_commit_rect: None,
            modal_cancel_rect: None,
            worktree_repo: None,
            worktree_error: None,
            mode: Mode::Normal,
            sidebars,
            module_docks: std::collections::HashMap::new(),
            module_dock_rects: Vec::new(),
            bar,
            zoomed: false,
            should_quit: false,
            server_mode: false,
            spinner: 0,
            session_dirty: false,
            events: api::new_bus(),
            orch: crate::orch::OrchState::load(),
            orch_scroll: 0,
            orch_cursor: 0,
            orch_form: None,
            orch_start: None,
            orch_detail: None,
            orch_detail_scroll: 0,
            orch_last_agent: 0,
            orch_area: Rect::ZERO,
            mission_scroll: 0,
            mission_cursor: 0,
            mission_scope: crate::mission::MissionScope::Workspace,
            mission_scope_rects: Vec::new(),
            mission_refresh_rect: None,
            mission_area: Rect::ZERO,
            mission_rows: Vec::new(),
            mission_detail: None,
            mission_answer: None,
            mission_burn: None,
            mission_last_cost: None,
            agent_usage: std::collections::HashMap::new(),
            usage_mtimes: std::collections::HashMap::new(),
            last_cursor: None,
            detach_requested: false,
            pending_session_switch: None,
            end_session: false,
            force_redraw: false,
            pending_notify: Vec::new(),
            pending_sound: false,
            selection: None,
            copy_mode: None,
            mouse_grab: None,
            pending_clipboard: None,
            pending_open_url: None,
            link_scan_at: None,
            hover_link: None,
            link_press: None,
            toast: None,
            downsample: false,
            last_cwd_at: Instant::now(),
            resumable: Vec::new(),
            sessions_scan_inflight: false,
            proc_commands: HashMap::new(),
            proc_scan_inflight: false,
            dismissed_sessions: HashSet::new(),
            last_sessions_at: Instant::now(),
            mission_usage_requested: false,
            mission_active_workspace: None,
            usage_scan_inflight: false,
            last_proc_at: Instant::now(),
            last_detect_at: Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(Instant::now),
            detection_dirty: HashSet::new(),
            last_detection_audit_at: Instant::now()
                .checked_sub(Duration::from_secs(2))
                .unwrap_or_else(Instant::now),
            detection_extractions: 0,
            detection_skips: 0,
            detection_panes_considered: 0,
            detection_full_fleet_audits: 0,
            detection_audit_recoveries: 0,
            output_waits: HashMap::new(),
            agent_waits: HashMap::new(),
            agent_starts: HashMap::new(),
            agent_prompts: HashMap::new(),
            last_output_wait_scan: Instant::now(),
            workspaces_scroll: 0,
            agents_scroll: 0,
            agents_active_only: false,
            workspaces_area: Rect::ZERO,
            agents_area: Rect::ZERO,
            // Rooted at nothing; the first detect tick re-roots it to the active
            // node (set_root is a no-op when already correct).
            file_tree: {
                // Start from the persisted show-hidden choice (docs/38). Read
                // from a hoisted local, since `config` is moved into its own
                // field above before this initializer runs.
                let mut t = crate::files::FileTree::new(std::path::PathBuf::new());
                t.show_hidden = files_show_hidden;
                t
            },
            pending_file_tree_api: Vec::new(),
            files_area: Rect::ZERO,
            file_tree_rects: Vec::new(),
            files_mode: crate::diff::FilesMode::Files,
            files_mode_rects: Vec::new(),
            diff_row_rects: Vec::new(),
            diff_source_rects: Vec::new(),
            diff_note_rects: Vec::new(),
            diff_note_drag: None,
            diff: crate::diff::DiffState::default(),
            diff_agent_picker: None,
            diff_menu: None,
            pending_diff_api: Vec::new(),
            views,
            editor_files: HashMap::new(),
            recent_files: VecDeque::new(),
            preview_views: HashSet::new(),
            file_git_status: HashMap::new(),
            git_status_inflight: false,
            last_git_status_at: Instant::now()
                .checked_sub(Duration::from_secs(10))
                .unwrap_or_else(Instant::now),
            last_file_scan_at: Instant::now()
                .checked_sub(Duration::from_secs(10))
                .unwrap_or_else(Instant::now),
            file_menu: None,
            dock_menu: None,
            file_prompt: None,
            file_delete: None,
            compact: false,
            switcher: false,
            search: None,
            search_flash: None,
            switcher_cursor: 0,
            switcher_scroll: 0,
            switcher_query: String::new(),
            switcher_scope: SwitcherScope::All,
            switcher_rects: Vec::new(),
            switcher_scope_rects: Vec::new(),
            switcher_button_rect: None,
            mobile_pane_prev_rect: None,
            mobile_pane_next_rect: None,
            switcher_close_rect: None,
            last_active_ws_shown: 0,
            hover: None,
            app_tx,
            last_pane_area: Rect::ZERO,
            pane_rects: Vec::new(),
            pane_content_rects: Vec::new(),
            scroll_pane: None,
            resize_drag: None,
            hover_divider: None,
            sidebar_resize: None,
            hover_sidebar: None,
            left_seam: None,
            right_seam: None,
            last_main_area: Rect::ZERO,
            tab_rects: Vec::new(),
            ws_rects: Vec::new(),
            workspace_branch_rects: Vec::new(),
            git_section_rects: Vec::new(),
            agents_filter_rects: Vec::new(),
            agent_rects: Vec::new(),
            session_rects: Vec::new(),
            tab_close_rects: Vec::new(),
            new_ws_rect: None,
            tab_prev_rect: None,
            tab_next_rect: None,
            pane_close_rect: None,
            pane_zoom_rect: None,
            sidebar_toggle_rect: None,
            right_sidebar_toggle_rect: None,
            version_rect: None,
            changelog_modal_rect: None,
            changelog_close_rect: None,
            changelog_check_rect: None,
            changelog_link_rects: Vec::new(),
            changelog_copy_rects: Vec::new(),
            changelog_rows: None,
            settings_icon_rect: None,
            settings_close_rect: None,
            settings_modal_rect: None,
            settings_tab_rects: Vec::new(),
            settings_ctl_rects: Vec::new(),
            settings_theme_remove_rects: Vec::new(),
            pending_theme_uninstalls: HashMap::new(),
            theme_selection_revision: 0,
            settings_arrow_rects: Vec::new(),
            modules,
            module_logs: Vec::new(),
            module_panes,
            module_startup_done: std::collections::HashSet::new(),
            module_setting_edit: None,
        };
        // Pane ids are reallocated every run, so the ledger's pane bindings from
        // the previous server are stale — rebind them to the restored panes (by
        // worktree cwd) or clear them, so the board never lies (docs/22).
        app.orch_reconcile();
        app.refresh_core_bar_widgets();
        Some(app)
    }

    /// Configure color output for the local terminal (downsample if no truecolor).
    pub fn set_color_mode(&mut self, truecolor: bool) {
        if !truecolor {
            self.downsample = true;
            self.theme = self.theme.to_256();
        }
    }

    /// Apply colors reported by the terminal displaying the foreground client.
    pub fn apply_terminal_colors(&mut self, colors: &crate::terminal::theme_probe::TerminalColors) {
        self.theme = crate::ui::theme::Theme::from_terminal(colors);
        if self.downsample {
            self.theme = self.theme.to_256();
        }
    }

    /// Set a sidebar's width, clamped to the supported range, and persist.
    pub fn set_side_width(&mut self, side: Side, cols: u16) {
        self.sidebars.get_mut(side).width = cols.clamp(SIDEBAR_WIDTH_MIN, SIDEBAR_WIDTH_MAX);
        self.save_sidebars();
    }

    /// Show/hide a sidebar (runtime-only; not persisted, so a session always
    /// starts from the configured layout). The `»`/`«` chevrons use this per side.
    pub fn toggle_side(&mut self, side: Side) {
        let s = self.sidebars.get_mut(side);
        s.visible = !s.visible;
    }

    /// Show/hide **both** sidebars at once (`Ctrl+Space b`): if either is showing,
    /// collapse both for a full-width view; otherwise bring both back. Runtime-only
    /// like [`toggle_side`](Self::toggle_side).
    pub fn toggle_all_sides(&mut self) {
        let target = !(self.sidebars.left.visible || self.sidebars.right.visible);
        self.sidebars.left.visible = target;
        self.sidebars.right.visible = target;
    }

    /// Write the current sidebar layout into `config` and persist it, mirroring
    /// the legacy `sidebar_width` from the left for safe downgrade (docs/29).
    pub fn save_sidebars(&mut self) {
        self.config.sidebars = Some(self.sidebars.to_config());
        self.config.sidebar_width = self.sidebars.left.width;
        crate::config::save(&self.config);
    }

    /// Every mounted dock in display order: left sidebar top→bottom, then right.
    pub fn docks_flat(&self) -> Vec<DockKind> {
        let mut v = self.sidebars.left.docks.clone();
        v.extend(self.sidebars.right.docks.clone());
        v
    }

    /// Move a dock to `target` (removed from its current side, appended to the
    /// target's end) and persist. A no-op if it is already the only place.
    ///
    /// Returns `false` without changing anything when `target` is already full
    /// (`MAX_DOCKS_PER_SIDE`, docs/29): the dock keeps its current spot and a toast
    /// asks the user to free a slot, rather than silently dropping it or overflowing
    /// the side. Placing a dock onto the side it already occupies is never "full".
    pub fn move_dock(&mut self, kind: &DockKind, target: Side) -> bool {
        let dst = self.sidebars.get(target);
        if !dst.has(kind) && dst.docks.len() >= MAX_DOCKS_PER_SIDE {
            let msg = self.catalog.sidebar_full;
            self.show_toast(msg);
            return false;
        }
        for side in [Side::Left, Side::Right] {
            self.sidebars.get_mut(side).docks.retain(|d| d != kind);
        }
        let dst = self.sidebars.get_mut(target);
        if !dst.docks.contains(kind) {
            dst.docks.push(kind.clone());
        }
        // Placing a module dock on a side is the user opting it back in, so clear
        // any explicit "off" flag (the inverse of `unmount_dock`).
        if let DockKind::Module(id) = kind {
            self.config.docks_off.retain(|d| d != id);
        }
        self.save_sidebars();
        true
    }

    /// The "off" state (docs/29): remove a dock from both sidebars so it shows
    /// nowhere, without dropping any module content cache (it stays in the
    /// registry and can be re-placed). Persists.
    pub fn unmount_dock(&mut self, kind: &DockKind) {
        for side in [Side::Left, Side::Right] {
            self.sidebars.get_mut(side).docks.retain(|d| d != kind);
        }
        // Remember an explicit "off" for a module dock so its own `ui.dock.push`
        // (startup / `workspace.created` / a refresh) can't resurrect it on the
        // next push or restart — without this, "off" is indistinguishable from
        // "never placed" and `push_module_dock` re-mounts it on its default side.
        if let DockKind::Module(id) = kind {
            if !self.config.docks_off.iter().any(|d| d == id) {
                self.config.docks_off.push(id.clone());
            }
        }
        self.save_sidebars();
    }

    /// Human label for a dock (localized for built-ins; the module dock's title
    /// for modules).
    pub fn dock_label(&self, kind: &DockKind) -> String {
        match kind {
            DockKind::Workspaces => self.catalog.workspaces.to_string(),
            DockKind::Agents => self.catalog.agents.to_string(),
            DockKind::Files => self.catalog.files.to_string(),
            DockKind::Module(id) => self.module_dock_title(id),
        }
    }

    /// A module dock's title: its pushed/cached title, else the title declared in
    /// an installed module's manifest, else the id (docs/29, DOCK-4).
    pub fn module_dock_title(&self, id: &str) -> String {
        if let Some(d) = self.module_docks.get(id) {
            return d.title.clone();
        }
        for m in &self.modules.modules {
            if let Some(d) = m.manifest.docks.iter().find(|d| d.id == id) {
                return d.title.clone();
            }
        }
        id.to_string()
    }

    /// The **dock registry** (docs/29): every dock the settings can place —
    /// built-ins plus every dock declared by an installed, runnable module, then
    /// live/configured fallback docks sorted by id. The fallback order is
    /// deliberately independent from sidebar placement: moving a dock appends it
    /// on that side, but must not move its Settings row under the pointer.
    /// Deduplicated, built-ins first. Its current side is `sidebars.side_of(kind)`
    /// (`None` = not placed yet).
    pub fn available_docks(&self) -> Vec<DockKind> {
        let mut v = vec![DockKind::Workspaces, DockKind::Agents, DockKind::Files];
        for m in self.modules.modules.iter().filter(|m| m.is_runnable()) {
            for d in &m.manifest.docks {
                let k = DockKind::Module(d.id.clone());
                if !v.contains(&k) {
                    v.push(k);
                }
            }
        }

        // A live API dock can exist without a currently-runnable declaration,
        // and an explicitly-off stale dock exists only in `docks_off`. Keep all
        // of them placeable, but sort this fallback set instead of deriving its
        // order from left/right sidebar vectors that placement mutates.
        let mut fallback: Vec<DockKind> = self
            .module_docks
            .keys()
            .chain(self.config.docks_off.iter())
            .map(|id| DockKind::Module(id.clone()))
            .chain(self.docks_flat())
            .collect();
        fallback.sort_by(|a, b| a.id().cmp(b.id()));
        fallback.dedup();
        for k in fallback {
            if !v.contains(&k) {
                v.push(k);
            }
        }
        v
    }

    /// Cache a module dock's content (`ui.dock.push`) and, the first time, mount
    /// it into `placement` so it appears without the user wiring it up (docs/29,
    /// DOCK-4). Subsequent pushes only refresh the rows/title.
    pub fn push_module_dock(
        &mut self,
        id: &str,
        title: Option<String>,
        placement: Side,
        rows: Vec<DockRow>,
    ) {
        let entry = self
            .module_docks
            .entry(id.to_string())
            .or_insert_with(|| ModuleDock {
                title: id.to_string(),
                rows: Vec::new(),
            });
        if let Some(tt) = title {
            entry.title = tt;
        }
        entry.rows = rows;
        let kind = DockKind::Module(id.to_string());
        // Auto-mount to the module's default side only if the user has not turned
        // it off, it is not already placed, and the side has a free slot;
        // otherwise the dock stays "off" (cached, placeable from Settings). The
        // `docks_off` check is what stops a re-push (startup / `workspace.created`
        // / refresh) from resurrecting a dock the user turned off (docs/29). We
        // check room here rather than letting `move_dock` reject it, so a module
        // pushing on startup never flashes the "sidebar full" toast at the user.
        let off = self.config.docks_off.iter().any(|d| d == id);
        if !off && self.sidebars.side_of(&kind).is_none() && self.sidebars.has_room(placement) {
            self.move_dock(&kind, placement);
        }
    }

    /// Remove module docks (by id) from both sidebars and drop their cache — on
    /// module disable / unlink / uninstall (docs/29, DOCK-4).
    pub fn remove_module_docks(&mut self, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        for id in ids {
            let kind = DockKind::Module(id.clone());
            for side in [Side::Left, Side::Right] {
                self.sidebars.get_mut(side).docks.retain(|d| d != &kind);
            }
            self.module_docks.remove(id);
        }
        self.save_sidebars();
    }

    // ── accessors ───────────────────────────────────────────────────────────

    /// True if any pane is currently Working — drives the sidebar spinner and
    /// how often the loop repaints to animate it.
    pub fn any_working(&self) -> bool {
        self.status
            .values()
            .any(|s| s.state == crate::ui::theme::State::Working)
    }

    /// Re-arm every pane's PTY wake-coalescing flag (see `Pane.data_pending`),
    /// letting the readers announce fresh output again. Returns whether any
    /// flag was set — output arrived since the last re-arm, so the caller may
    /// owe one more render for the tail of a burst. Non-short-circuiting `|`:
    /// every flag must be consumed.
    pub fn rearm_pty_notify(&self) -> bool {
        self.panes
            .values()
            .fold(false, |any, p| any | p.take_data_pending())
    }

    /// Re-arm PTY notifications while distinguishing output visible in the
    /// active tab from output owned by another tab or workspace. The server
    /// uses this to keep focused rendering responsive without repeatedly
    /// diffing an unchanged UI for background-only bursts.
    pub fn rearm_pty_notify_by_visibility(&self) -> (bool, bool) {
        let layout = self.workspaces.get(self.active_ws).and_then(|workspace| {
            workspace
                .tabs
                .get(workspace.active_tab)
                .map(|tab| &tab.layout)
        });
        let mut visible = false;
        let mut background = false;
        for (id, pane) in &self.panes {
            if !pane.take_data_pending() {
                continue;
            }
            if layout.is_some_and(|layout| layout.contains(*id)) {
                visible = true;
            } else {
                background = true;
            }
        }
        (visible, background)
    }

    /// Whether any PTY reader is currently coalescing an output notification.
    pub fn has_pending_pty_output(&self) -> bool {
        self.panes.values().any(|pane| pane.has_data_pending())
    }

    /// Whether a pane is rendered in the active tab.
    pub fn pane_is_visible(&self, id: PaneId) -> bool {
        self.workspaces
            .get(self.active_ws)
            .and_then(|workspace| workspace.tabs.get(workspace.active_tab))
            .is_some_and(|tab| tab.layout.contains(id))
    }

    pub fn ws(&self) -> &Workspace {
        &self.workspaces[self.active_ws]
    }

    pub fn layout(&self) -> &TileLayout {
        let ws = self.ws();
        &ws.tabs[ws.active_tab].layout
    }

    fn layout_mut(&mut self) -> &mut TileLayout {
        let ws = &mut self.workspaces[self.active_ws];
        let at = ws.active_tab;
        &mut ws.tabs[at].layout
    }

    pub fn focused(&self) -> Option<&Pane> {
        self.panes.get(&self.layout().focus)
    }

    fn focused_cwd(&self) -> PathBuf {
        self.focused()
            .map(|p| p.cwd.clone())
            .unwrap_or_else(|| self.ws().cwd.clone())
    }

    /// The ordered chain of directories a newly opened tab or split may start
    /// in: the first that still exists is where it lands, and the rest are
    /// fallbacks the deferred worker retries if that directory vanishes before
    /// it forks. By default the chain is the focused pane's live cwd, then the
    /// workspace root, then `$HOME`, so a new pane starts where the user is
    /// working; with `layout.new_pane_to_workspace_root` set it starts at the
    /// workspace root instead. Only existing directories are kept (de-duplicated,
    /// order preserved), so a deleted workspace root is never handed back as a
    /// dead cwd; `$HOME` anchors the chain when nothing nearer survives, matching
    /// the synchronous spawn path's own fallback. Shared by `new_tab` and `split`
    /// so the two stay aligned.
    fn spawn_cwds(&self) -> Vec<PathBuf> {
        let home = crate::platform::home_dir().unwrap_or_default();
        let root = self.ws().cwd.clone();
        let ordered = if self.config.layout.new_pane_to_workspace_root {
            vec![root, home.clone()]
        } else {
            vec![self.focused_cwd(), root, home.clone()]
        };
        let mut chain: Vec<PathBuf> = Vec::new();
        for candidate in ordered {
            if candidate.is_dir() && !chain.contains(&candidate) {
                chain.push(candidate);
            }
        }
        if chain.is_empty() {
            chain.push(home);
        }
        chain
    }

    // ── mutations ─────────────────────────────────────────────────────────────

    fn spawn_into(&mut self, cwd: PathBuf) -> Option<PaneId> {
        let id = PaneId::alloc();
        let shell = crate::platform::resolve_shell(&self.config.shell);
        let history_budget_bytes = self.config.scrollback_bytes();
        match Pane::spawn(
            id,
            80,
            24,
            cwd,
            self.app_tx.clone(),
            None,
            &shell,
            history_budget_bytes,
        ) {
            Ok(pane) => {
                let cmd = pane.command.clone();
                self.panes.insert(id, pane);
                self.status.insert(id, PaneStatus::new(cmd));
                self.zoomed = false;
                self.session_dirty = true;
                self.emit_event(
                    "pane.created",
                    serde_json::json!({"pane": id.0.to_string()}),
                );
                Some(id)
            }
            Err(_) => None,
        }
    }

    /// `spawn_into`, but with the shell forked off-loop (docs/82): the pane
    /// exists immediately so `pane split` answers before paying the fork.
    /// Used only where synchronous failure reporting is not required — the
    /// sync `spawn_into` keeps `workspace.open`'s "shell failed to start"
    /// contract.
    fn spawn_into_deferred(&mut self, cwd: PathBuf, fallback_cwds: &[PathBuf]) -> Option<PaneId> {
        let id = PaneId::alloc();
        let shell = crate::platform::resolve_shell(&self.config.shell);
        let history_budget_bytes = self.config.scrollback_bytes();
        let pane = Pane::spawn_deferred(
            id,
            80,
            24,
            cwd,
            fallback_cwds,
            self.app_tx.clone(),
            &shell,
            history_budget_bytes,
        );
        let cmd = pane.command.clone();
        self.panes.insert(id, pane);
        self.status.insert(id, PaneStatus::new(cmd));
        self.zoomed = false;
        self.session_dirty = true;
        self.emit_event(
            "pane.created",
            serde_json::json!({"pane": id.0.to_string()}),
        );
        Some(id)
    }

    /// `spawn_into`, but queues an agent resume/fork command. POSIX and custom
    /// shells receive it through a normal interactive PTY so profile-managed
    /// executables are available; PowerShell can launch it directly.
    fn spawn_resume_pane(&mut self, cwd: PathBuf, resume: &str) -> Option<PaneId> {
        let id = PaneId::alloc();
        let shell = crate::platform::resolve_shell(&self.config.shell);
        let history_budget_bytes = self.config.scrollback_bytes();
        let argv = crate::platform::shell_run_then_interactive(&shell, resume.trim());
        let spawned = match &argv {
            Some(a) => Pane::spawn_shell_with(
                id,
                80,
                24,
                cwd,
                self.app_tx.clone(),
                None,
                &shell,
                a,
                history_budget_bytes,
            ),
            None => Pane::spawn(
                id,
                80,
                24,
                cwd,
                self.app_tx.clone(),
                None,
                &shell,
                history_budget_bytes,
            ),
        };
        match spawned {
            Ok(pane) => {
                if argv.is_none() {
                    pane.send(resume.as_bytes());
                }
                let cmd = pane.command.clone();
                self.panes.insert(id, pane);
                self.status.insert(id, PaneStatus::new(cmd));
                self.zoomed = false;
                self.session_dirty = true;
                self.emit_event(
                    "pane.created",
                    serde_json::json!({"pane": id.0.to_string()}),
                );
                Some(id)
            }
            Err(_) => None,
        }
    }

    fn split(&mut self, axis: Axis) {
        // Resolve the candidate chain up front (focused pane → workspace root →
        // $HOME, existing only) and hand the primary plus its fallbacks to the
        // deferred worker. If the primary is deleted in the race window before
        // the fork, the worker retries the fallbacks instead of spawning a dead
        // pane; the chain is never empty, so the `else` here is unreachable.
        let cwds = self.spawn_cwds();
        let Some((cwd, fallback_cwds)) = cwds.split_first() else {
            return;
        };
        if let Some(id) = self.spawn_into_deferred(cwd.clone(), fallback_cwds) {
            self.layout_mut().split_focused(axis, id);
        }
    }

    /// Fork `pane`'s agent session into a new sibling on its right, preserving
    /// the parent conversation. The source is resolved globally so Mission
    /// Control and socket callers can fork an inactive pane safely. With
    /// `focus=false`, the current UI focus and zoom state are preserved.
    pub fn fork_agent_pane(
        &mut self,
        pane: PaneId,
        focus: bool,
    ) -> Result<AgentForkResult, AgentForkError> {
        if !self.panes.contains_key(&pane) {
            return Err(AgentForkError::PaneNotFound);
        }
        let (wsi, ti) = self
            .workspaces
            .iter()
            .enumerate()
            .find_map(|(wi, ws)| {
                ws.tabs
                    .iter()
                    .position(|tab| tab.layout.leaves().contains(&pane))
                    .map(|tab| (wi, tab))
            })
            .ok_or(AgentForkError::SourceNotPaneTab)?;
        if !self.workspaces[wsi].tabs[ti].is_renameable() {
            return Err(AgentForkError::SourceNotPaneTab);
        }

        let st = self
            .status
            .get(&pane)
            .ok_or(AgentForkError::UnsupportedAgent)?;
        let agent = st.agent.clone();
        if !crate::agent::can_fork(&agent) {
            return Err(AgentForkError::UnsupportedAgent);
        }
        let cwd = self
            .panes
            .get(&pane)
            .map(|p| p.cwd.clone())
            .ok_or(AgentForkError::PaneNotFound)?;
        let sid = crate::agent::fork_session_id(
            &agent,
            st.agent_session.as_ref().map(|s| s.session_id.as_str()),
            &cwd,
        )
        .ok_or(AgentForkError::SessionUnknown)?;
        let fork =
            crate::agent::fork_command(&agent, &sid).ok_or(AgentForkError::UnsupportedAgent)?;

        // `spawn_resume_pane` changes the global zoom flag. Capture the complete
        // view state needed by --no-focus before spawning, then restore it after
        // inserting the new leaf into the source tab.
        let previous_zoom = self.zoomed;
        let previous_source_focus = self.workspaces[wsi].tabs[ti].layout.focus;
        let new_id = self
            .spawn_resume_pane(cwd, &fork)
            .ok_or(AgentForkError::SpawnFailed)?;
        {
            let layout = &mut self.workspaces[wsi].tabs[ti].layout;
            layout.focus = pane;
            layout.split_focused(Axis::Col, new_id);
            if !focus {
                layout.focus = previous_source_focus;
            }
        }
        if focus {
            self.active_ws = wsi;
            self.workspaces[wsi].active_tab = ti;
            self.scroll_pane = None;
            self.zoomed = false;
        } else {
            self.zoomed = previous_zoom;
        }
        // Label the new pane as the same agent right away (detection will confirm
        // it, and pick up the fork's fresh session id, on the next tick).
        if let Some(nst) = self.status.get_mut(&new_id) {
            nst.agent = agent.clone();
        }
        // Pin the *source* pane to the session we just forked from. The fork is
        // about to create another session in the same folder, and disk discovery
        // deliberately refuses ambiguous ownership at save time. Recording this
        // exact binding keeps the parent resumable until the fork reports its own
        // session. A hook report still wins because we only fill an empty binding.
        if let Some(st) = self.status.get_mut(&pane) {
            if st.agent_session.is_none() {
                st.agent_session = Some(AgentSession {
                    agent: agent.clone(),
                    session_id: sid.clone(),
                });
            }
        }
        self.session_dirty = true;
        self.show_toast(format!("forked {agent} session"));
        self.emit_event(
            "pane.forked",
            serde_json::json!({
                "from": pane.0.to_string(),
                "to": new_id.0.to_string(),
                "agent": agent,
                "workspace": wsi.to_string(),
                "tab": (ti + 1).to_string(),
            }),
        );
        Ok(AgentForkResult {
            from: pane,
            pane: new_id,
            agent,
            workspace: wsi,
            tab: ti,
        })
    }

    /// TUI compatibility wrapper: the existing pane/mission actions fork and
    /// follow the new sibling, while unsupported panes remain a quiet no-op.
    pub fn fork_pane(&mut self, pane: PaneId) -> bool {
        self.fork_agent_pane(pane, true).is_ok()
    }

    fn new_tab(&mut self) {
        // A new tab starts where the user is: the first existing directory in the
        // candidate chain (focused pane's cwd → workspace root → $HOME), the same
        // way `split` and `new_workspace` do, or the workspace root when
        // `layout.new_pane_to_workspace_root` is set. The synchronous spawn path
        // falls back to $HOME internally, so the chain's head is enough here. The
        // workspace root itself stays fixed, so the static-workspace model holds.
        let Some(cwd) = self.spawn_cwds().into_iter().next() else {
            return;
        };
        if let Some(id) = self.spawn_into(cwd) {
            let ws = &mut self.workspaces[self.active_ws];
            ws.tabs.push(Tab::panes(TileLayout::new(id)));
            ws.active_tab = ws.tabs.len() - 1;
            let tab = self.ws().active_tab + 1;
            self.emit_event("tab.created", serde_json::json!({"tab": tab.to_string()}));
        }
    }

    fn new_workspace(&mut self) {
        // No path chosen (CLI / fallback): use the current directory.
        let cwd = self.focused_cwd();
        self.create_workspace_at(cwd);
    }

    /// Change only a workspace's display label. The folder on disk remains
    /// untouched. The same validation is shared by the TUI and UHP.
    pub fn rename_workspace(
        &mut self,
        index: usize,
        raw_name: &str,
    ) -> Result<(), WorkspaceUpdateError> {
        let name = raw_name.trim();
        if name.is_empty() {
            return Err(WorkspaceUpdateError::EmptyName);
        }
        if name.chars().count() > WS_NAME_MAX {
            return Err(WorkspaceUpdateError::NameTooLong);
        }
        let workspace = self
            .workspaces
            .get_mut(index)
            .ok_or(WorkspaceUpdateError::NotFound)?;
        if workspace.name != name {
            workspace.name = name.to_string();
            self.session_dirty = true;
        }
        Ok(())
    }

    /// Pin or unpin a workspace in the sidebar. Indices stay in storage order;
    /// only [`Self::workspace_display_order`] changes.
    pub fn set_workspace_pinned(
        &mut self,
        index: usize,
        pinned: bool,
    ) -> Result<(), WorkspaceUpdateError> {
        let workspace = self
            .workspaces
            .get_mut(index)
            .ok_or(WorkspaceUpdateError::NotFound)?;
        if workspace.pinned != pinned {
            workspace.pinned = pinned;
            self.session_dirty = true;
        }
        Ok(())
    }

    /// Open `cwd` as a new **static** workspace (a workspace) and focus it. The folder
    /// is fixed — its name/cwd won't change as the pane's process `cd`s around.
    ///
    /// Returns whether the node was opened. Opening fails when the shell can't
    /// spawn there (a folder that vanished, no permission, a bad `config.shell`).
    /// That used to be swallowed: the caller carried on, `active_ws` still
    /// pointed at whatever was focused before, and the user saw the *previous*
    /// folder with no error anywhere — indistinguishable from luvus ignoring
    /// them. A toast is raised here so every caller reports it the same way.
    pub fn create_workspace_at(&mut self, cwd: PathBuf) -> bool {
        let name = ws_name(&cwd);
        let branch = git_branch(&cwd);
        let Some(id) = self.spawn_into(cwd.clone()) else {
            self.show_toast(format!("couldn't open {} — shell failed to start", name));
            return false;
        };
        self.workspaces.push(Workspace {
            id: crate::ids::public_id("workspace"),
            name,
            worktree: worktree_membership(&cwd),
            cwd,
            branch,
            git_ahead_behind: None,
            pinned: false,
            tabs: vec![Tab::panes(TileLayout::new(id))],
            active_tab: 0,
        });
        self.active_ws = self.workspaces.len() - 1;
        let ws = self.active_ws;
        self.emit_event(
            "workspace.created",
            serde_json::json!({"workspace": ws.to_string()}),
        );
        true
    }

    /// The order the WORKSPACES sidebar draws nodes in: each linked worktree is
    /// grouped **right under the node it branched from** (its main checkout, by
    /// shared git common dir) instead of at its raw creation position, so a
    /// worktree and its parent stay visually correlated. Returns `(index,
    /// is_member)` where `index` is the position in `self.workspaces` (kept for
    /// hit-testing) and `is_member` marks a nested worktree row.
    pub fn workspace_display_order(&self) -> Vec<(usize, bool)> {
        let order = {
            let nodes: Vec<(Option<&std::path::Path>, bool)> = self
                .workspaces
                .iter()
                .map(|w| {
                    (
                        w.worktree.as_ref().map(|m| m.common_dir.as_path()),
                        w.worktree.as_ref().is_some_and(|m| m.linked),
                    )
                })
                .collect();
            group_worktrees(&nodes)
        };
        // Pinned nodes (with their worktree group) float to the top. A pin on
        // either the parent or a linked child pins the complete group, while a
        // stable sort preserves internal group order and the unpinned order.
        let mut group_pinned = vec![false; self.workspaces.len()];
        let mut leader = 0usize;
        for &(idx, is_child) in &order {
            if !is_child {
                leader = idx;
            }
            if self.workspaces.get(idx).is_some_and(|w| w.pinned) {
                group_pinned[leader] = true;
            }
        }
        leader = 0;
        let mut keyed: Vec<(bool, usize, bool)> = order
            .into_iter()
            .map(|(idx, is_child)| {
                if !is_child {
                    leader = idx;
                }
                let pinned = group_pinned.get(leader).copied().unwrap_or(false);
                (!pinned, idx, is_child) // !pinned => pinned groups sort first
            })
            .collect();
        keyed.sort_by_key(|&(k, _, _)| k);
        keyed
            .into_iter()
            .map(|(_, idx, is_child)| (idx, is_child))
            .collect()
    }

    /// A workspace's 0-based position in the sidebar after pin/group ordering.
    pub fn workspace_display_position(&self, index: usize) -> Option<usize> {
        self.workspace_display_order()
            .iter()
            .position(|&(workspace, _)| workspace == index)
    }

    /// Create a git worktree for `branch` off `repo` and open it as a workspace
    /// (docs/18 WT). Laid out **nested by repo** —
    /// `~/.luvus/worktrees/<repo>/<branch>` — so checkouts don't clutter the repo
    /// and stay readable, with a numeric suffix if that path is taken (two repos
    /// of the same name, or `feat/x` vs `feat-x` both slugging to `feat-x`).
    /// Returns the new worktree path.
    pub fn create_worktree(
        &mut self,
        repo: &std::path::Path,
        branch: &str,
    ) -> Result<PathBuf, String> {
        let branch = branch.trim();
        if branch.is_empty() {
            return Err("a branch name is required".into());
        }
        if !crate::git::local::is_repo(repo) {
            return Err("not a git repository".into());
        }
        // Nest under the **main** worktree's name, so every checkout of one repo
        // groups under a single folder even when you branch off another worktree.
        let base = worktrees_dir_for(repo);
        let _ = std::fs::create_dir_all(&base);
        // `git worktree add` requires the target not to exist, so pick the first
        // free `<branch>` / `<branch>-2` / `<branch>-3` … under the repo folder.
        let slug = branch.replace(['/', ' '], "-");
        let mut path = base.join(&slug);
        let mut n = 2;
        while path.exists() {
            path = base.join(format!("{slug}-{n}"));
            n += 1;
        }
        crate::git::local::worktree_add(repo, &path, branch)?;
        self.create_workspace_at(path.clone());
        Ok(path)
    }

    /// Open the new-worktree branch prompt (`Ctrl+Space G`) for the active workspace,
    /// if it's a git repo (worktrees only make sense inside one).
    pub fn open_worktree_prompt(&mut self) {
        let cwd = self.ws().cwd.clone();
        if crate::git::local::is_repo(&cwd) {
            self.worktree_repo = Some(cwd);
            self.worktree_prompt = Some(String::new());
        }
    }

    /// Open the rename modal for tab `index` (docs/28). No-op for the git/orch
    /// dashboards or the `+` button (index past the last tab).
    pub fn open_tab_rename(&mut self, index: usize) {
        let workspace = self.active_ws;
        if let Some(tab) = self.workspaces[workspace].tabs.get(index) {
            if tab.is_renameable() {
                let buffer = tab.name.clone().unwrap_or_default();
                if let Some(target) = self.tab_menu_target(workspace, index) {
                    self.tab_rename = Some(TabRename { target, buffer });
                }
            }
        }
    }

    /// Key handling while the tab-rename modal is open. `Enter` commits (an empty
    /// name clears the custom name, reverting to the number); `Esc` cancels.
    pub fn handle_tab_rename_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.tab_rename = None,
            KeyCode::Enter => {
                if let Some(r) = self.tab_rename.take() {
                    let target = self.resolve_tab_menu_target(&r.target);
                    if let Some((workspace, index)) = target {
                        let _ = self.rename_tab_in_workspace(workspace, index, &r.buffer);
                    } else {
                        self.show_toast(self.catalog.tab_changed_rename_cancelled);
                    }
                }
            }
            KeyCode::Backspace => {
                if let Some(r) = self.tab_rename.as_mut() {
                    r.buffer.pop();
                }
            }
            KeyCode::Char(c) => {
                if let Some(r) = self.tab_rename.as_mut() {
                    if r.buffer.chars().count() < TAB_NAME_MAX {
                        r.buffer.push(c);
                    }
                }
            }
            _ => {}
        }
    }

    // ── tab context menu (right-click a tab) ──

    fn tab_menu_target(&self, workspace: usize, index: usize) -> Option<TabMenuTarget> {
        let tab = self.workspaces.get(workspace)?.tabs.get(index)?;
        Some(TabMenuTarget {
            workspace,
            leaves: tab.layout.leaves(),
        })
    }

    fn resolve_tab_menu_target(&self, target: &TabMenuTarget) -> Option<(usize, usize)> {
        let ws = self.workspaces.get(target.workspace)?;
        let index = ws
            .tabs
            .iter()
            .position(|tab| tab.layout.leaves() == target.leaves)?;
        Some((target.workspace, index))
    }

    fn tab_menu_label(tab: &Tab, index: usize) -> String {
        if let Some(name) = tab.name.as_deref() {
            name.to_string()
        } else if tab.is_git() {
            "git".to_string()
        } else if tab.is_orch() {
            "orch".to_string()
        } else if tab.is_mission() {
            "ctrl".to_string()
        } else {
            format!("#{}", index + 1)
        }
    }

    /// Open the context menu for the exact tab that was right-clicked. The `+`
    /// button shares `tab_rects`, so an index past the real tabs is rejected.
    pub fn open_tab_menu(&mut self, index: usize, col: u16, row: u16) {
        let workspace = self.active_ws;
        let Some(target) = self.tab_menu_target(workspace, index) else {
            return;
        };
        let ws = &self.workspaces[workspace];
        let tab = &ws.tabs[index];
        let can_rename = tab.is_renameable();
        let can_move_left = index > 0;
        let can_move_right = index + 1 < ws.tabs.len();
        let swap_targets = ws
            .tabs
            .iter()
            .enumerate()
            .filter(|(other, _)| *other != index)
            .filter_map(|(other, tab)| {
                self.tab_menu_target(workspace, other)
                    .map(|target| (target, Self::tab_menu_label(tab, other)))
            })
            .collect();
        self.tab_menu = Some(TabMenu {
            target,
            anchor: (col, row),
            items: Vec::new(),
            module_actions: self.module_menu_actions("tab"),
            can_rename,
            can_move_left,
            can_move_right,
            swap_targets,
            swap_open: false,
            swap_rects: Vec::new(),
        });
    }

    /// Stable rows for the open tab menu. Availability is snapshotted at open so
    /// a background socket request cannot shift which action an old hitbox means.
    pub fn tab_menu_items(&self) -> Vec<TabMenuItem> {
        let Some(menu) = self.tab_menu.as_ref() else {
            return Vec::new();
        };
        let mut items = Vec::new();
        if menu.can_rename {
            items.push(TabMenuItem::Rename);
        }
        if menu.can_move_left {
            items.push(TabMenuItem::MoveLeft);
        }
        if menu.can_move_right {
            items.push(TabMenuItem::MoveRight);
        }
        if !menu.swap_targets.is_empty() {
            items.push(TabMenuItem::SwapWith);
        }
        if !menu.module_actions.is_empty() {
            if !items.is_empty() {
                items.push(TabMenuItem::Divider);
            }
            items.extend((0..menu.module_actions.len()).map(TabMenuItem::Module));
        }
        items
    }

    /// Route a click to the Swap With submenu or the main tab-menu rows.
    pub fn tab_menu_click(&mut self, col: u16, row: u16) {
        let in_rect = |r: &Rect| col >= r.x && col < r.right() && row >= r.y && row < r.bottom();
        let swap_hit = self.tab_menu.as_ref().and_then(|menu| {
            menu.swap_rects
                .iter()
                .find(|(_, rect)| in_rect(rect))
                .map(|(target, _)| (menu.target.clone(), target.clone()))
        });
        if let Some((source, target)) = swap_hit {
            self.tab_menu = None;
            let source = self.resolve_tab_menu_target(&source);
            let target = self.resolve_tab_menu_target(&target);
            match (source, target) {
                (Some((workspace, from)), Some((target_workspace, to)))
                    if workspace == target_workspace =>
                {
                    let _ = self.swap_tabs_in_workspace(workspace, from, to);
                }
                _ => self.show_toast(self.catalog.tab_changed_reopen_menu),
            }
            return;
        }

        let hit = self.tab_menu.as_ref().and_then(|menu| {
            menu.items
                .iter()
                .find(|(_, rect)| in_rect(rect))
                .map(|(item, _)| *item)
        });
        match hit {
            Some(TabMenuItem::SwapWith) => {
                if let Some(menu) = self.tab_menu.as_mut() {
                    menu.swap_open = true;
                }
            }
            Some(TabMenuItem::Divider) => {}
            Some(item) => self.tab_menu_action(item),
            None => self.tab_menu = None,
        }
    }

    /// Run a tab-menu action on the snapshotted tab, then close the menu.
    pub fn tab_menu_action(&mut self, item: TabMenuItem) {
        let Some((target, actions)) = self
            .tab_menu
            .as_ref()
            .map(|menu| (menu.target.clone(), menu.module_actions.clone()))
        else {
            return;
        };
        self.tab_menu = None;
        let Some((workspace, index)) = self.resolve_tab_menu_target(&target) else {
            self.show_toast(self.catalog.tab_changed_reopen_menu);
            return;
        };
        match item {
            TabMenuItem::Rename => {
                if workspace == self.active_ws {
                    self.open_tab_rename(index);
                }
            }
            TabMenuItem::MoveLeft => {
                if self
                    .move_tab_direction_in_workspace(workspace, index, TabMoveDirection::Left)
                    .is_err()
                {
                    self.show_toast(self.catalog.tab_cannot_move_left);
                }
            }
            TabMenuItem::MoveRight => {
                if self
                    .move_tab_direction_in_workspace(workspace, index, TabMoveDirection::Right)
                    .is_err()
                {
                    self.show_toast(self.catalog.tab_cannot_move_right);
                }
            }
            TabMenuItem::SwapWith | TabMenuItem::Divider => {}
            TabMenuItem::Module(i) => {
                if let Some(action) = actions.get(i).cloned() {
                    self.run_module_menu_action("tab", action, Target::tab(workspace, index));
                }
            }
        }
    }

    pub fn handle_tab_menu_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.tab_menu = None;
        }
    }

    // ── workspace context menu (right-click a WORKSPACES row) ──

    /// Open the workspace context menu for row `index`, anchored at the cursor.
    pub fn open_ws_menu(&mut self, index: usize, col: u16, row: u16) {
        if index < self.workspaces.len() {
            let is_repo = crate::git::local::is_repo(&self.workspaces[index].cwd);
            self.ws_menu = Some(WsMenu {
                workspace_id: self.workspaces[index].id.clone(),
                is_repo,
                anchor: (col, row),
                items: Vec::new(),
                module_actions: self.module_menu_actions("workspace"),
            });
        }
    }

    /// The items shown for workspace `index`, in render order: node actions
    /// (close / rename / worktrees) above a divider, then the open-tab actions
    /// (git / orch). Worktree + git actions only appear for nodes in a git repo.
    pub fn ws_menu_items(&self, index: usize) -> Vec<WsMenuItem> {
        let ws = self.workspaces.get(index);
        let is_repo = self
            .ws_menu
            .as_ref()
            .filter(|menu| {
                ws.is_some_and(|workspace| workspace.id.as_str() == menu.workspace_id.as_str())
            })
            .map(|menu| menu.is_repo)
            // Keep this helper useful to callers that inspect rows before
            // opening a menu. The renderer always takes the cached branch.
            .unwrap_or_else(|| ws.is_some_and(|w| crate::git::local::is_repo(&w.cwd)));
        // A linked worktree (a `git worktree add` checkout) can be deleted; a main
        // checkout or plain workspace cannot — only closed.
        let is_worktree = ws
            .and_then(|w| w.worktree.as_ref())
            .is_some_and(|m| m.linked);
        let pin = if ws.is_some_and(|w| w.pinned) {
            WsMenuItem::Unpin
        } else {
            WsMenuItem::Pin
        };
        let mut items = vec![WsMenuItem::Close, WsMenuItem::Rename, pin];
        if is_worktree {
            items.push(WsMenuItem::DeleteWorktree);
        }
        if is_repo {
            items.push(WsMenuItem::NewWorktree);
            items.push(WsMenuItem::OpenWorktree);
        }
        items.push(WsMenuItem::Divider);
        if is_repo {
            items.push(WsMenuItem::OpenGit);
        }
        items.push(WsMenuItem::OpenOrch);
        items.push(WsMenuItem::OpenMission);
        // Module actions declaring `contexts = ["workspace"]`, below a divider.
        let extras = self.ws_menu.as_ref().map_or(0, |m| m.module_actions.len());
        if extras > 0 {
            items.push(WsMenuItem::Divider);
            items.extend((0..extras).map(WsMenuItem::Module));
        }
        items
    }

    /// Resolve the open menu's stable target after any API-driven workspace
    /// mutation. A missing target means the menu is stale and must be dismissed.
    pub fn ws_menu_target_index(&self) -> Option<usize> {
        let target = &self.ws_menu.as_ref()?.workspace_id;
        self.workspaces
            .iter()
            .position(|workspace| workspace.id == *target)
    }

    /// The pane context-menu items: the built-ins, then any module actions
    /// declaring `contexts = ["pane"]` below a divider. "Move to tab" is dropped
    /// when there's nowhere to move to.
    pub fn pane_menu_items(&self) -> Vec<PaneMenuItem> {
        let has_move = self
            .pane_menu
            .as_ref()
            .is_some_and(|m| !m.move_targets.is_empty());
        let can_fork = self.pane_menu.as_ref().is_some_and(|m| m.can_fork);
        let (has_url, has_file) = match self.pane_menu.as_ref().and_then(|m| m.link.as_ref()) {
            Some(LinkTarget::Url(_)) => (true, false),
            Some(LinkTarget::File { .. }) => (false, true),
            None => (false, false),
        };
        let mut items: Vec<PaneMenuItem> = PaneMenuItem::ALL
            .iter()
            .copied()
            .filter(|it| has_move || *it != PaneMenuItem::MoveToTab)
            .filter(|it| can_fork || *it != PaneMenuItem::ForkPane)
            .filter(|it| has_url || *it != PaneMenuItem::OpenLink)
            .filter(|it| has_file || *it != PaneMenuItem::OpenFile)
            .collect();
        let extras = self
            .pane_menu
            .as_ref()
            .map_or(0, |m| m.module_actions.len());
        if extras > 0 {
            items.push(PaneMenuItem::Divider);
            items.extend((0..extras).map(PaneMenuItem::Module));
        }
        items
    }

    /// The AGENTS-list context-menu items for `target`, plus module actions
    /// declaring `contexts = ["agent"]`.
    pub fn agent_menu_items(&self, target: AgentTarget) -> Vec<AgentMenuItem> {
        let mut items = AgentMenu::items_for(target);
        // A live agent can be pinned to the top of the AGENTS list, below its
        // Rename/Close actions (per-session, since pane ids are reallocated).
        if let AgentTarget::Live(id) = target {
            items.push(if self.pinned_agents.contains(&id) {
                AgentMenuItem::Unpin
            } else {
                AgentMenuItem::Pin
            });
        }
        let extras = self
            .agent_menu
            .as_ref()
            .map_or(0, |m| m.module_actions.len());
        if extras > 0 {
            items.push(AgentMenuItem::Divider);
            items.extend((0..extras).map(AgentMenuItem::Module));
        }
        items
    }

    /// A click inside the open context menu: run the hit item, else dismiss.
    pub fn ws_menu_click(&mut self, col: u16, row: u16) {
        let hit = self.ws_menu.as_ref().and_then(|m| {
            m.items
                .iter()
                .find(|(_, r)| col >= r.x && col < r.right() && row >= r.y && row < r.bottom())
                .map(|(it, _)| *it)
        });
        match hit {
            Some(WsMenuItem::Divider) => {} // non-interactive; keep the menu open
            Some(it) => self.ws_menu_action(it),
            None => self.ws_menu = None, // click outside dismisses
        }
    }

    /// Run a context-menu action for the menu's target, then close the menu.
    pub fn ws_menu_action(&mut self, item: WsMenuItem) {
        let Some((workspace_id, actions)) = self
            .ws_menu
            .as_ref()
            .map(|m| (m.workspace_id.clone(), m.module_actions.clone()))
        else {
            return;
        };
        self.ws_menu = None;
        let Some(index) = self
            .workspaces
            .iter()
            .position(|workspace| workspace.id == workspace_id)
        else {
            return;
        };
        let cwd = self.workspaces.get(index).map(|w| w.cwd.clone());
        match item {
            WsMenuItem::Divider => {}
            // Pin/Unpin the right-clicked node: float it to the top of the list
            // (docs), persisted across restarts.
            WsMenuItem::Pin | WsMenuItem::Unpin => {
                let _ = self.set_workspace_pinned(index, item == WsMenuItem::Pin);
            }
            // The right-clicked node, which needn't be the focused one.
            WsMenuItem::Module(i) => {
                if let Some(a) = actions.get(i).cloned() {
                    self.run_module_menu_action("workspace", a, Target::workspace(index));
                }
            }
            WsMenuItem::Rename => self.open_ws_rename(index),
            WsMenuItem::Close => self.close_workspace(index),
            // Destructive: arm the confirm modal rather than delete immediately.
            WsMenuItem::DeleteWorktree => self.worktree_delete = Some(workspace_id),
            WsMenuItem::NewWorktree => {
                if let Some(cwd) = cwd.filter(|p| crate::git::local::is_repo(p)) {
                    self.worktree_repo = Some(cwd);
                    self.worktree_prompt = Some(String::new());
                    self.worktree_error = None;
                }
            }
            WsMenuItem::OpenWorktree => {
                if let Some(cwd) = cwd.filter(|p| crate::git::local::is_repo(p)) {
                    // Land in this repo's worktrees folder so its checkouts list.
                    let wt = worktrees_dir_for(&cwd);
                    let start = if wt.is_dir() { wt } else { cwd };
                    self.open_folder_picker_at(start);
                }
            }
            // Both switch to the node first, then open (or focus) its dashboard.
            WsMenuItem::OpenGit => self.open_git_tab(index), // no-op for non-repos
            WsMenuItem::OpenOrch => {
                if index < self.workspaces.len() {
                    self.active_ws = index;
                    self.open_orch_board();
                }
            }
            WsMenuItem::OpenMission => self.open_mission_control(index),
        }
    }

    /// Keys for the worktree-delete confirm modal: `y`/⏎ deletes, anything else
    /// cancels (mirrors `file_delete_key`).
    pub fn worktree_delete_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                self.confirm_worktree_delete()
            }
            _ => self.worktree_delete = None,
        }
    }

    /// Delete the armed worktree: `git worktree remove --force` (its branch is
    /// kept), a folder-removal fallback if git leaves it, then drop the node.
    /// Guarded so it only ever acts on a **linked worktree**, never a main
    /// checkout.
    fn confirm_worktree_delete(&mut self) {
        let Some(workspace_id) = self.worktree_delete.take() else {
            return;
        };
        let Some(index) = self
            .workspaces
            .iter()
            .position(|workspace| workspace.id == workspace_id)
        else {
            return;
        };
        // Extract owned paths under an immutable borrow, then act (mutable).
        let target = self.workspaces.get(index).and_then(|ws| {
            ws.worktree.as_ref().filter(|m| m.linked).map(|m| {
                // Run git from the repo's main working tree (the common dir's
                // parent), so it never refuses "from inside the worktree".
                let repo = m
                    .common_dir
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| ws.cwd.clone());
                (ws.cwd.clone(), repo)
            })
        });
        let Some((path, repo)) = target else {
            self.show_toast("not a worktree");
            return;
        };
        match crate::git::local::worktree_remove_force(&repo, &path) {
            Ok(()) => {
                if path.exists() {
                    let _ = std::fs::remove_dir_all(&path);
                }
                self.close_workspace(index);
                self.show_toast("worktree deleted");
            }
            Err(e) => self.show_toast(format!("delete failed: {e}")),
        }
    }

    /// Open the rename modal for workspace `index`, pre-filled with its label.
    pub fn open_ws_rename(&mut self, index: usize) {
        if let Some(w) = self.workspaces.get(index) {
            self.ws_rename = Some(WsRename {
                workspace_id: w.id.clone(),
                buffer: w.name.clone(),
            });
        }
    }

    /// Set (`Some`) or clear (`None`) a pane's live name. One name per pane, one
    /// pane per name. Shared by `agent.name`, `pane name`, and the rename modal.
    pub fn set_agent_name(&mut self, pane: PaneId, name: Option<&str>) {
        match name {
            None => {
                self.agent_names.retain(|_, x| *x != pane);
            }
            Some(n) => {
                self.agent_names.retain(|k, x| k != n && *x != pane);
                self.agent_names.insert(n.to_string(), pane);
            }
        }
        // Persist the change so the name survives a restart (docs/06).
        self.session_dirty = true;
    }

    /// Open the pane-rename modal for `pane`, pre-filled with its current name.
    pub fn open_pane_rename(&mut self, pane: PaneId) {
        let buffer = self
            .agent_names
            .iter()
            .find_map(|(name, target)| (*target == pane).then_some(name.as_str()))
            .unwrap_or("")
            .to_string();
        self.pane_rename = Some(PaneRename { pane, buffer });
    }

    /// Key handling while the pane-rename modal is open. `Enter` applies the name
    /// (empty clears it), `Esc` cancels. Typing is restricted to the addressable
    /// grammar, so the buffer is always a valid name.
    pub fn handle_pane_rename_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.pane_rename = None,
            KeyCode::Enter => {
                if let Some(r) = self.pane_rename.take() {
                    let name = r.buffer.trim();
                    self.set_agent_name(r.pane, (!name.is_empty()).then_some(name));
                }
            }
            KeyCode::Backspace => {
                if let Some(r) = self.pane_rename.as_mut() {
                    r.buffer.pop();
                }
            }
            KeyCode::Char(c) => {
                if let Some(r) = self.pane_rename.as_mut() {
                    let c = c.to_ascii_lowercase();
                    let char_ok =
                        c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-';
                    // A name must start with a letter.
                    let first_ok = !r.buffer.is_empty() || c.is_ascii_lowercase();
                    if char_ok && first_ok && r.buffer.chars().count() < PANE_NAME_MAX {
                        r.buffer.push(c);
                    }
                }
            }
            _ => {}
        }
    }

    /// Key handling while the workspace-rename modal is open (mirrors tab rename).
    /// `Enter` commits a non-empty name (the on-disk folder is never renamed);
    /// `Esc` cancels.
    pub fn handle_ws_rename_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.ws_rename = None,
            KeyCode::Enter => {
                if let Some(r) = self.ws_rename.take() {
                    if let Some(index) = self
                        .workspaces
                        .iter()
                        .position(|workspace| workspace.id == r.workspace_id)
                    {
                        let _ = self.rename_workspace(index, &r.buffer);
                    }
                }
            }
            KeyCode::Backspace => {
                if let Some(r) = self.ws_rename.as_mut() {
                    r.buffer.pop();
                }
            }
            KeyCode::Char(c) => {
                if let Some(r) = self.ws_rename.as_mut() {
                    if r.buffer.chars().count() < WS_NAME_MAX {
                        r.buffer.push(c);
                    }
                }
            }
            _ => {}
        }
    }

    /// Key handling while the workspace context menu is open: `Esc` closes it.
    pub fn handle_ws_menu_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.ws_menu = None;
        }
    }

    /// Open the pane context menu (split / close) for `pane`, anchored at the
    /// click. No-op on a dashboard tab (no real panes to act on).
    pub fn open_pane_menu(&mut self, pane: PaneId, col: u16, row: u16) {
        if self.active_is_git() || self.active_is_orch() || self.active_is_mission() {
            return;
        }
        let move_targets = self.pane_move_targets();
        let can_fork = self.can_fork_pane(pane);
        let link = self.link_at_screen(col, row).map(|h| h.target);
        self.pane_menu = Some(PaneMenu {
            pane,
            anchor: (col, row),
            items: Vec::new(),
            module_actions: self.module_menu_actions("pane"),
            move_targets,
            move_open: false,
            tab_rects: Vec::new(),
            can_fork,
            link,
        });
    }

    /// Whether `pane` has both a native fork implementation and a safe source
    /// session. Codex deliberately has no cwd-based fallback: several rollouts
    /// commonly share a directory, so showing Fork without its exact binding
    /// would offer an action that must fail or fork the wrong conversation.
    fn can_fork_pane(&self, pane: PaneId) -> bool {
        let Some(st) = self.status.get(&pane) else {
            return false;
        };
        if !crate::agent::can_fork(&st.agent) {
            return false;
        }
        let Some(cwd) = self.panes.get(&pane).map(|pane| &pane.cwd) else {
            return false;
        };
        crate::agent::fork_session_id(
            &st.agent,
            st.agent_session
                .as_ref()
                .map(|session| session.session_id.as_str()),
            cwd,
        )
        .is_some()
    }

    /// The tabs this pane could move into: every other real pane tab in the
    /// workspace (not the current one, not a dashboard), then a fresh tab. Empty
    /// when there's nowhere useful to move (one pane in one tab).
    fn pane_move_targets(&self) -> Vec<(MoveTarget, String)> {
        let wsi = self.active_ws;
        let cur = self.workspaces[wsi].active_tab;
        let mut targets = Vec::new();
        for (ti, tab) in self.workspaces[wsi].tabs.iter().enumerate() {
            if ti == cur || !tab.is_renameable() {
                continue;
            }
            // A named tab shows its name; an unnamed one its number (as in the tab
            // bar), which needs no translation.
            let label = tab.name.clone().unwrap_or_else(|| format!("#{}", ti + 1));
            targets.push((MoveTarget::Tab(ti), label));
        }
        // Offer "new tab" only when it would actually separate panes or there are
        // other tabs — i.e. don't offer a pointless move for a lone pane.
        let other_tabs = !targets.is_empty();
        let many_panes = self.workspaces[wsi]
            .tabs
            .get(cur)
            .is_some_and(|t| t.layout.len() > 1);
        if other_tabs || many_panes {
            targets.push((MoveTarget::NewTab, self.catalog.menu_new_tab.to_string()));
        }
        targets
    }

    /// Move `pane` into another tab in its current workspace, keeping its process
    /// alive. Validation happens before the source layout is touched, so a stale
    /// socket request cannot detach a pane. Existing-tab indices refer to the
    /// pre-move tab order. If the source empties it is removed; focus follows the
    /// pane, matching the TUI context-menu behavior.
    pub fn move_pane_to_tab(
        &mut self,
        pane: PaneId,
        target: MoveTarget,
    ) -> Result<PaneMoveResult, PaneMoveError> {
        let (wsi, src) = self
            .workspaces
            .iter()
            .enumerate()
            .find_map(|(wi, ws)| {
                ws.tabs
                    .iter()
                    .position(|tab| tab.layout.leaves().contains(&pane))
                    .map(|ti| (wi, ti))
            })
            .ok_or(PaneMoveError::PaneNotFound)?;

        if !self.workspaces[wsi].tabs[src].is_renameable() {
            return Err(PaneMoveError::SourceNotPaneTab);
        }
        match target {
            MoveTarget::Tab(ti) => {
                let tabs = &self.workspaces[wsi].tabs;
                if ti >= tabs.len() {
                    return Err(PaneMoveError::TargetOutOfRange);
                }
                if ti == src {
                    return Err(PaneMoveError::SameTab);
                }
                if !tabs[ti].is_renameable() {
                    return Err(PaneMoveError::TargetNotPaneTab);
                }
            }
            MoveTarget::NewTab => {
                let source_is_lone_tab = self.workspaces[wsi].tabs.len() == 1
                    && self.workspaces[wsi].tabs[src].layout.len() == 1;
                if source_is_lone_tab {
                    return Err(PaneMoveError::NoChange);
                }
            }
        }

        // Detach only after every fallible check has passed. The pane remains in
        // `App.panes`; only its layout-tree parent changes.
        let emptied = self.workspaces[wsi].tabs[src].layout.remove(pane);
        let final_tab = match target {
            MoveTarget::Tab(mut ti) => {
                if emptied {
                    self.workspaces[wsi].tabs.remove(src);
                    if src < ti {
                        ti -= 1;
                    }
                }
                self.workspaces[wsi].tabs[ti]
                    .layout
                    .split_focused(Axis::Col, pane);
                ti
            }
            MoveTarget::NewTab => {
                if emptied {
                    self.workspaces[wsi].tabs.remove(src);
                }
                self.workspaces[wsi]
                    .tabs
                    .push(Tab::panes(TileLayout::new(pane)));
                self.workspaces[wsi].tabs.len() - 1
            }
        };

        self.active_ws = wsi;
        self.workspaces[wsi].active_tab = final_tab;
        self.workspaces[wsi].tabs[final_tab].layout.focus = pane;
        self.zoomed = false;
        self.scroll_pane = None;
        self.session_dirty = true;
        self.emit_event(
            "pane.moved",
            serde_json::json!({
                "pane": pane.0.to_string(),
                "workspace": wsi.to_string(),
                "tab": (final_tab + 1).to_string(),
            }),
        );
        Ok(PaneMoveResult {
            workspace: wsi,
            tab: final_tab,
        })
    }

    /// Move a tab within the active workspace. `from` and `to` are zero-based
    /// final positions. The active tab follows the same tab object rather than
    /// staying on the old numeric slot.
    pub fn move_tab(&mut self, from: usize, to: usize) -> Result<usize, TabMoveError> {
        self.move_tab_in_workspace(self.active_ws, from, to)
    }

    /// Swap two tab positions in the active workspace. The active tab follows
    /// the same tab object when either swapped position contains it.
    pub fn swap_tabs(&mut self, first: usize, second: usize) -> Result<usize, TabMoveError> {
        self.swap_tabs_in_workspace(self.active_ws, first, second)
    }

    /// Move one tab by one adjacent position. `from = None` targets the active
    /// tab, which powers `luvus tab move left|right`; an explicit zero-based
    /// source powers `--tab N` and the right-click menu.
    pub fn move_tab_direction(
        &mut self,
        from: Option<usize>,
        direction: TabMoveDirection,
    ) -> Result<(usize, usize, usize), TabMoveError> {
        let from = from.unwrap_or(self.ws().active_tab);
        let to = self.adjacent_tab_position(self.active_ws, from, direction)?;
        let active = self.move_tab_in_workspace(self.active_ws, from, to)?;
        Ok((from, to, active))
    }

    fn move_tab_direction_in_workspace(
        &mut self,
        workspace: usize,
        from: usize,
        direction: TabMoveDirection,
    ) -> Result<usize, TabMoveError> {
        let to = self.adjacent_tab_position(workspace, from, direction)?;
        self.move_tab_in_workspace(workspace, from, to)
    }

    fn adjacent_tab_position(
        &self,
        workspace: usize,
        from: usize,
        direction: TabMoveDirection,
    ) -> Result<usize, TabMoveError> {
        let len = self
            .workspaces
            .get(workspace)
            .map(|ws| ws.tabs.len())
            .ok_or(TabMoveError::PositionOutOfRange)?;
        if from >= len {
            return Err(TabMoveError::PositionOutOfRange);
        }
        match direction {
            TabMoveDirection::Left => from.checked_sub(1).ok_or(TabMoveError::AlreadyFirst),
            TabMoveDirection::Right if from + 1 < len => Ok(from + 1),
            TabMoveDirection::Right => Err(TabMoveError::AlreadyLast),
        }
    }

    fn move_tab_in_workspace(
        &mut self,
        workspace: usize,
        from: usize,
        to: usize,
    ) -> Result<usize, TabMoveError> {
        let Some(ws) = self.workspaces.get(workspace) else {
            return Err(TabMoveError::PositionOutOfRange);
        };
        let len = ws.tabs.len();
        if from >= len || to >= len {
            return Err(TabMoveError::PositionOutOfRange);
        }
        if from == to {
            return Err(TabMoveError::SamePosition);
        }

        let active = ws.active_tab;
        let new_active = {
            let ws = &mut self.workspaces[workspace];
            let tab = ws.tabs.remove(from);
            ws.tabs.insert(to, tab);
            ws.active_tab = if active == from {
                to
            } else if from < active && active <= to {
                active - 1
            } else if to <= active && active < from {
                active + 1
            } else {
                active
            };
            ws.active_tab
        };
        self.session_dirty = true;
        self.emit_event(
            "tab.moved",
            serde_json::json!({
                "workspace": workspace.to_string(),
                "from": (from + 1).to_string(),
                "to": (to + 1).to_string(),
                "active": (new_active + 1).to_string(),
            }),
        );
        Ok(new_active)
    }

    fn swap_tabs_in_workspace(
        &mut self,
        workspace: usize,
        first: usize,
        second: usize,
    ) -> Result<usize, TabMoveError> {
        let Some(ws) = self.workspaces.get(workspace) else {
            return Err(TabMoveError::PositionOutOfRange);
        };
        if first >= ws.tabs.len() || second >= ws.tabs.len() {
            return Err(TabMoveError::PositionOutOfRange);
        }
        if first == second {
            return Err(TabMoveError::SamePosition);
        }

        let new_active = {
            let ws = &mut self.workspaces[workspace];
            ws.tabs.swap(first, second);
            ws.active_tab = if ws.active_tab == first {
                second
            } else if ws.active_tab == second {
                first
            } else {
                ws.active_tab
            };
            ws.active_tab
        };
        self.session_dirty = true;
        self.emit_event(
            "tab.moved",
            serde_json::json!({
                "workspace": workspace.to_string(),
                "from": (first + 1).to_string(),
                "to": (second + 1).to_string(),
                "active": (new_active + 1).to_string(),
                "mode": "swap",
            }),
        );
        Ok(new_active)
    }

    /// A click inside the open pane menu: a submenu tab (move the pane), the
    /// "Move to tab" row (open the submenu), another item (run it), else dismiss.
    pub fn pane_menu_click(&mut self, col: u16, row: u16) {
        let in_rect = |r: &Rect| col >= r.x && col < r.right() && row >= r.y && row < r.bottom();
        // A submenu tab row → move the pane there.
        let tab_hit = self.pane_menu.as_ref().and_then(|m| {
            m.tab_rects
                .iter()
                .find(|(_, r)| in_rect(r))
                .map(|(tg, _)| *tg)
        });
        if let Some(tg) = tab_hit {
            if let Some(pane) = self.pane_menu.as_ref().map(|m| m.pane) {
                self.pane_menu = None;
                let _ = self.move_pane_to_tab(pane, tg);
            }
            return;
        }
        let hit = self
            .pane_menu
            .as_ref()
            .and_then(|m| m.items.iter().find(|(_, r)| in_rect(r)).map(|(it, _)| *it));
        match hit {
            // Open the submenu; the tabs list appears to the side.
            Some(PaneMenuItem::MoveToTab) => {
                if let Some(m) = self.pane_menu.as_mut() {
                    m.move_open = true;
                }
            }
            Some(PaneMenuItem::Divider) => {} // non-interactive; keep the menu open
            Some(it) => self.pane_menu_action(it),
            None => self.pane_menu = None, // click outside dismisses
        }
    }

    /// Run a pane context-menu action on its target pane, then close the menu.
    pub fn pane_menu_action(&mut self, item: PaneMenuItem) {
        let Some((pane, actions, link)) = self
            .pane_menu
            .as_ref()
            .map(|m| (m.pane, m.module_actions.clone(), m.link.clone()))
        else {
            return;
        };
        self.pane_menu = None;
        // Act on the right-clicked pane, not whatever was focused before.
        self.layout_mut().focus = pane;
        match item {
            PaneMenuItem::Divider => {}
            PaneMenuItem::Module(i) => {
                if let Some(a) = actions.get(i).cloned() {
                    let mut target = Target::pane(pane);
                    target.selection = self.selection_text();
                    self.run_module_menu_action("pane", a, target);
                }
            }
            PaneMenuItem::SplitVertical => self.split(Axis::Col), // side by side
            PaneMenuItem::SplitHorizontal => self.split(Axis::Row), // stacked
            PaneMenuItem::ForkPane => {
                self.fork_pane(pane);
            }
            PaneMenuItem::OpenLink | PaneMenuItem::OpenFile => {
                if let Some(t) = link {
                    self.activate_link(t);
                }
            }
            PaneMenuItem::RunningCmd => self.open_cmd_inspect(pane),
            PaneMenuItem::RenamePane => self.open_pane_rename(pane),
            // Handled in `pane_menu_click` (opens a submenu, keeps the menu open);
            // reachable here only via a direct call — treat as a no-op.
            PaneMenuItem::MoveToTab => {}
            PaneMenuItem::Close => self.close_pane(pane),
        }
    }

    /// Key handling while the pane context menu is open: `Esc` closes it.
    pub fn handle_pane_menu_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.pane_menu = None;
        }
    }

    /// Open the AGENTS-list context menu for `target` (a resumable session or a
    /// live agent), anchored at the click.
    pub fn open_agent_menu(&mut self, target: AgentTarget, col: u16, row: u16) {
        // Only a live agent has a pane for an action to act on.
        let module_actions = match target {
            AgentTarget::Live(_) => self.module_menu_actions("agent"),
            AgentTarget::Session(_) => Vec::new(),
        };
        self.agent_menu = Some(AgentMenu {
            target,
            anchor: (col, row),
            items: Vec::new(),
            module_actions,
        });
    }

    /// A click inside the open AGENTS menu: run the hit item, else dismiss.
    pub fn agent_menu_click(&mut self, col: u16, row: u16) {
        let hit = self.agent_menu.as_ref().and_then(|m| {
            m.items
                .iter()
                .find(|(_, r)| col >= r.x && col < r.right() && row >= r.y && row < r.bottom())
                .map(|(it, _)| *it)
        });
        match hit {
            Some(AgentMenuItem::Divider) => {} // non-interactive; keep the menu open
            Some(it) => self.agent_menu_action(it),
            None => self.agent_menu = None, // click outside dismisses
        }
    }

    /// Run an AGENTS-menu action, then close the menu. Resume/Close act on a
    /// session; Close on a live agent jumps to and closes its pane.
    pub fn agent_menu_action(&mut self, item: AgentMenuItem) {
        let Some((target, actions)) = self
            .agent_menu
            .as_ref()
            .map(|m| (m.target, m.module_actions.clone()))
        else {
            return;
        };
        self.agent_menu = None;
        match (item, target) {
            (AgentMenuItem::Resume, AgentTarget::Session(i)) => self.resume_session(i),
            (AgentMenuItem::Close, AgentTarget::Session(i)) => self.dismiss_session(i),
            (AgentMenuItem::Close, AgentTarget::Live(id)) => {
                self.focus_pane_global(id); // switch to its tab so close targets it
                self.close_pane(id);
            }
            (AgentMenuItem::RenamePane, AgentTarget::Live(id)) => self.open_pane_rename(id),
            (AgentMenuItem::RenamePane, AgentTarget::Session(_)) => {} // no live pane
            (AgentMenuItem::Pin, AgentTarget::Live(id)) => {
                self.pinned_agents.insert(id);
            }
            (AgentMenuItem::Unpin, AgentTarget::Live(id)) => {
                self.pinned_agents.remove(&id);
            }
            (AgentMenuItem::Pin | AgentMenuItem::Unpin, AgentTarget::Session(_)) => {} // no pane
            (AgentMenuItem::Resume, AgentTarget::Live(_)) => {} // n/a for a live agent
            (AgentMenuItem::Module(i), AgentTarget::Live(id)) => {
                if let Some(a) = actions.get(i).cloned() {
                    self.run_module_menu_action("agent", a, Target::pane(id));
                }
            }
            (AgentMenuItem::Module(_), AgentTarget::Session(_)) => {} // no live pane
            (AgentMenuItem::Divider, _) => {}
        }
    }

    /// Key handling while the AGENTS menu is open: `Esc` closes it.
    pub fn handle_agent_menu_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.agent_menu = None;
        }
    }

    /// Key handling while the new-worktree prompt is open.
    pub fn handle_worktree_prompt_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.worktree_prompt = None;
                self.worktree_repo = None;
                self.worktree_error = None;
            }
            KeyCode::Enter => {
                let branch = self.worktree_prompt.clone().unwrap_or_default();
                if let Some(repo) = self.worktree_repo.clone() {
                    match self.create_worktree(&repo, &branch) {
                        Ok(_) => {
                            // Success: close the prompt; the new workspace is focused.
                            self.worktree_prompt = None;
                            self.worktree_repo = None;
                            self.worktree_error = None;
                        }
                        // Failure (branch already checked out, dirty tree, empty
                        // name…): keep the prompt open and show why, so it's never
                        // a silent no-op.
                        Err(e) => self.worktree_error = Some(e),
                    }
                } else {
                    self.worktree_prompt = None;
                }
            }
            KeyCode::Backspace => {
                if let Some(b) = self.worktree_prompt.as_mut() {
                    b.pop();
                }
                self.worktree_error = None;
            }
            KeyCode::Char(c) => {
                if let Some(b) = self.worktree_prompt.as_mut() {
                    b.push(c);
                }
                self.worktree_error = None;
            }
            _ => {}
        }
    }

    /// Focus an exact zero-based tab position in the active workspace.
    pub fn focus_tab(&mut self, index: usize) -> Result<(), TabFocusError> {
        self.focus_tab_in_workspace(self.active_ws, index)
    }

    /// Rename an exact zero-based tab in the active workspace. Empty text clears
    /// the custom label; dashboards retain their fixed product labels.
    pub fn rename_tab(&mut self, index: usize, name: &str) -> Result<(), TabRenameError> {
        self.rename_tab_in_workspace(self.active_ws, index, name)
    }

    fn rename_tab_in_workspace(
        &mut self,
        workspace: usize,
        index: usize,
        name: &str,
    ) -> Result<(), TabRenameError> {
        let name = name.trim();
        if name.chars().count() > TAB_NAME_MAX {
            return Err(TabRenameError::NameTooLong);
        }
        let tab = self
            .workspaces
            .get_mut(workspace)
            .and_then(|ws| ws.tabs.get_mut(index))
            .ok_or(TabRenameError::PositionOutOfRange)?;
        if !tab.is_renameable() {
            return Err(TabRenameError::Dashboard);
        }
        tab.name = (!name.is_empty()).then(|| name.to_string());
        self.session_dirty = true;
        Ok(())
    }

    fn focus_tab_in_workspace(
        &mut self,
        workspace: usize,
        index: usize,
    ) -> Result<(), TabFocusError> {
        if self
            .workspaces
            .get(workspace)
            .is_none_or(|ws| index >= ws.tabs.len())
        {
            return Err(TabFocusError::PositionOutOfRange);
        }
        self.active_ws = workspace;
        self.workspaces[workspace].active_tab = index;
        Ok(())
    }

    fn switch_tab(&mut self, i: usize) {
        let _ = self.focus_tab(i);
    }

    fn cycle_tab(&mut self, delta: isize) {
        let ws = &mut self.workspaces[self.active_ws];
        let n = ws.tabs.len() as isize;
        if n > 0 {
            ws.active_tab = (((ws.active_tab as isize + delta) % n + n) % n) as usize;
        }
    }

    /// Track each pane's live process cwd (used for per-pane git / agent-session
    /// keying) and refresh each workspace's git branch from its **fixed** folder.
    /// A workspace is a **static workspace**: `cd`-ing inside a pane does not move the
    /// workspace's directory — only its branch updates (a checkout changes that).
    fn refresh_cwds(&mut self) {
        let updates: Vec<(PaneId, PathBuf)> = self
            .panes
            .iter()
            .filter_map(|(id, p)| {
                let pid = p.child_pid.load(std::sync::atomic::Ordering::SeqCst);
                (pid != 0)
                    .then(|| crate::platform::process_cwd(pid))
                    .flatten()
                    .map(|c| (*id, c))
            })
            .collect();
        for (id, cwd) in updates {
            if let Some(p) = self.panes.get_mut(&id) {
                p.cwd = cwd;
            }
        }
        let branches: Vec<(usize, Option<String>)> = self
            .workspaces
            .iter()
            .enumerate()
            .map(|(wi, ws)| (wi, git_branch(&ws.cwd)))
            .collect();
        for (wi, branch) in branches {
            if let Some(ws) = self.workspaces.get_mut(wi) {
                ws.branch = branch;
            }
        }
    }

    /// Rescan the agents' on-disk session stores for sessions you can reopen,
    /// dropping any whose project already has that agent running live, and any
    /// the user has dismissed from the list.
    /// Synchronous rescan — used by on-demand API calls (`agent.sessions`) and
    /// tests. The periodic path in `detect_tick` runs the same scan on a worker
    /// thread instead and applies it via [`Self::apply_scanned_sessions`].
    fn refresh_resumable(&mut self) {
        let found = crate::agent::recent_sessions(12);
        self.apply_scanned_sessions(found);
    }

    /// Fold a finished process scan into `proc_commands`, re-keyed from child
    /// pids to panes. Returns whether the scan directly changed visible agent
    /// lifecycle state. Process-cache churn alone stays clean because the next
    /// detection tick decides whether it changes an identity or status. A
    /// `None` result means the scan could not run at all, so the previous mapping
    /// is *kept* rather than cleared — dropping it would silently demote every
    /// agent back to text-only detection.
    pub(crate) fn apply_proc_scan(&mut self, found: Option<HashMap<u32, Vec<String>>>) -> bool {
        self.proc_scan_inflight = false;
        let Some(by_pid) = found else { return false };
        let mut next: HashMap<PaneId, Vec<String>> = HashMap::new();
        for (id, pane) in self.panes.iter() {
            let pid = pane.child_pid.load(std::sync::atomic::Ordering::SeqCst);
            if let Some(cmds) = (pid != 0).then(|| by_pid.get(&pid)).flatten() {
                next.insert(*id, cmds.clone());
            }
        }
        let mut lifecycle_changed = false;
        for (id, cmds) in &next {
            // A successful Unix scan includes at least the pane's shell. Empty
            // means the process table could not see this root, so it is not
            // evidence that an agent exited.
            if cmds.is_empty() {
                continue;
            }
            let base = self
                .panes
                .get(id)
                .map(|p| p.command.clone())
                .unwrap_or_default();
            let Some(st) = self.status.get_mut(id) else {
                continue;
            };
            let Some(bound_agent) = st.agent_session.as_ref().map(|s| s.agent.as_str()) else {
                st.agent_absent_scans = 0;
                continue;
            };
            if self.manifests.process_has_agent(cmds, bound_agent) {
                st.agent_absent_scans = 0;
                continue;
            }

            st.agent_absent_scans = st.agent_absent_scans.saturating_add(1);
            if st.agent_absent_scans < 2 {
                continue;
            }

            // The agent process has been absent from two real process-table
            // snapshots. Treat that as an intentional/complete return to the
            // shell: keeping the session here would make persistence relaunch
            // it after a detach + later restart. If another recognised agent is
            // already running, publish that identity while its own session hook
            // catches up; otherwise this is now a plain shell pane.
            st.agent_session = None;
            st.agent_absent_scans = 0;
            st.agent = self.manifests.agent_in_processes(cmds).unwrap_or(base);
            lifecycle_changed = true;
        }
        let processes_changed = self.proc_commands != next;
        self.proc_commands = next;
        if processes_changed {
            for status in self.status.values_mut() {
                status.force_detect = true;
            }
        }
        if lifecycle_changed {
            self.session_dirty = true;
        }
        lifecycle_changed
    }

    /// Fold a finished session scan into the sidebar list. Returns whether the
    /// visible list changed (→ repaint). Also prunes `dismissed_sessions` to
    /// ids the scan still sees, so the set can't grow for the life of the
    /// server (a dismissal only means anything while its session is on disk).
    pub(crate) fn apply_scanned_sessions(&mut self, found: Vec<crate::agent::SessionInfo>) -> bool {
        self.sessions_scan_inflight = false;
        let on_disk: HashSet<&str> = found.iter().map(|s| s.session_id.as_str()).collect();
        self.dismissed_sessions
            .retain(|id| on_disk.contains(id.as_str()));
        let open: HashSet<(String, PathBuf)> = self
            .status
            .iter()
            .filter(|(_, s)| crate::agent::is_resumable(&s.agent))
            .filter_map(|(id, s)| self.panes.get(id).map(|p| (s.agent.clone(), p.cwd.clone())))
            .collect();
        let dismissed = &self.dismissed_sessions;
        let fresh: Vec<crate::agent::SessionInfo> = found
            .into_iter()
            .filter(|s| {
                !dismissed.contains(&s.session_id)
                    && !open.contains(&(s.agent.clone(), s.cwd.clone()))
            })
            .collect();
        let changed = fresh.len() != self.resumable.len()
            || fresh
                .iter()
                .zip(&self.resumable)
                .any(|(a, b)| a.session_id != b.session_id);
        self.resumable = fresh;
        changed
    }

    /// Remove a resumable session from the sidebar list. Hides it for the rest of
    /// the run (so the periodic rescan doesn't bring it back) — it does NOT touch
    /// the agent's stored session on disk.
    pub fn dismiss_session(&mut self, idx: usize) {
        if idx >= self.resumable.len() {
            return;
        }
        let s = self.resumable.remove(idx);
        self.dismissed_sessions.insert(s.session_id);
    }

    /// Reopen a resumable session (from the AGENTS sidebar): spawn a pane in the
    /// session's directory — reusing its workspace if one exists, else a new workspace —
    /// and run the agent's resume command.
    pub fn resume_session(&mut self, idx: usize) {
        let Some(s) = self.resumable.get(idx).cloned() else {
            return;
        };
        let Some(resume) = crate::agent::resume_command(&s.agent, &s.session_id) else {
            return;
        };
        let Some(id) = self.spawn_resume_pane(s.cwd.clone(), &resume) else {
            return;
        };
        let tab = Tab::panes(TileLayout::new(id));
        // Per the Layout setting, reuse the session's own workspace (or the workspace at
        // its cwd); otherwise open it as a tab in the currently active workspace.
        let target = if self.config.layout.resume_in_new_workspace {
            self.workspaces
                .iter()
                .position(|w| crate::platform::same_path(&w.cwd, &s.cwd))
        } else {
            Some(self.active_ws)
        };
        if let Some(wi) = target {
            self.active_ws = wi;
            let ws = &mut self.workspaces[wi];
            ws.tabs.push(tab);
            ws.active_tab = ws.tabs.len() - 1;
        } else {
            let branch = git_branch(&s.cwd);
            self.workspaces.push(Workspace {
                id: crate::ids::public_id("workspace"),
                name: ws_name(&s.cwd),
                cwd: s.cwd.clone(),
                branch,
                git_ahead_behind: None,
                pinned: false,
                worktree: worktree_membership(&s.cwd),
                tabs: vec![tab],
                active_tab: 0,
            });
            self.active_ws = self.workspaces.len() - 1;
        }
        if let Some(st) = self.status.get_mut(&id) {
            st.agent = s.agent.clone();
            st.agent_session = Some(AgentSession {
                agent: s.agent.clone(),
                session_id: s.session_id.clone(),
            });
        }
        self.mode = Mode::Normal;
        self.resumable.retain(|r| r.session_id != s.session_id);
    }

    /// Focus a pane anywhere (used when clicking an agent in the global list).
    /// The node a pane lives in, or `None` if the pane is already gone. Used to
    /// label a pane with its node (name / branch) in the API and events.
    pub fn workspace_of_pane(&self, id: PaneId) -> Option<&Workspace> {
        self.workspaces
            .iter()
            .find(|ws| ws.tabs.iter().any(|t| t.layout.leaves().contains(&id)))
    }

    /// The reusable preview owned by the active workspace, if it is still a
    /// live native view. Preview identity is workspace-scoped: focusing a
    /// preview must never change the active workspace as a side effect.
    fn active_preview_view(&self) -> Option<PaneId> {
        let workspace = self.ws();
        self.preview_views.iter().copied().find(|id| {
            self.views.contains_key(id) && workspace.tabs.iter().any(|tab| tab.layout.contains(*id))
        })
    }

    fn focus_pane_global(&mut self, id: PaneId) {
        let changed = self.layout().focus != id;
        let mut found = None;
        for (wi, ws) in self.workspaces.iter().enumerate() {
            for (ti, tab) in ws.tabs.iter().enumerate() {
                if tab.layout.leaves().contains(&id) {
                    found = Some((wi, ti));
                }
            }
        }
        if let Some((wi, ti)) = found {
            self.active_ws = wi;
            self.workspaces[wi].active_tab = ti;
            self.workspaces[wi].tabs[ti].layout.focus = id;
            if changed {
                self.scroll_pane = None;
            }
            self.mode = Mode::Normal;
        }
    }

    fn cycle_workspace(&mut self, delta: isize) {
        let n = self.workspaces.len() as isize;
        if n > 0 {
            self.active_ws = (((self.active_ws as isize + delta) % n + n) % n) as usize;
        }
    }

    fn focus_dir(&mut self, dir: Dir) {
        let area = self.last_pane_area;
        self.layout_mut().focus_dir(area, dir);
    }

    /// Cycle focus within the current tab's leaf order, wrapping at both ends.
    /// Used by tmux-style `o` and the mobile header's previous/next halves.
    fn cycle_pane(&mut self, delta: isize) {
        let leaves = self.layout().leaves();
        if leaves.len() < 2 {
            return;
        }
        let focus = self.layout().focus;
        let idx = leaves.iter().position(|&id| id == focus).unwrap_or(0);
        let len = leaves.len() as isize;
        let next = leaves[((idx as isize + delta) % len + len) as usize % leaves.len()];
        self.layout_mut().focus = next;
        self.scroll_pane = None;
        self.mode = Mode::Normal;
    }

    fn focus_next_pane(&mut self) {
        self.cycle_pane(1);
    }

    // ── pane resize (docs/27) ───────────────────────────────────────────────

    /// Start a divider drag if `(c, r)` grabs one (RESIZE-2). Returns whether a
    /// drag began, so the mouse handler can skip selection/focus.
    /// The focused pane's close button sits on the top-right **border** cell,
    /// which for a stacked pane lands exactly on the horizontal divider. Resize
    /// must yield there, or the divider grab zone swallows every click on the ✕
    /// and the pane can't be closed by mouse.
    /// True if `(c, r)` lands on a pane's interactive **border chrome**: a title
    /// strip (the command-inspector button), the ⤢/⤡ zoom toggle, or the ✕ close
    /// button. These live on the top border row — exactly the cells a resize grab
    /// band would otherwise swallow — so every resize path (grab, Ctrl-grab, hover
    /// highlight) excludes them: the seam between panes stays grabbable, but a
    /// click on a title or button always wins. On a stacked split this matters
    /// most, since the divider line *is* the lower pane's top border.
    fn on_pane_chrome(&self, c: u16, r: u16) -> bool {
        fn hit(rc: Rect, c: u16, r: u16) -> bool {
            c >= rc.x && c < rc.right() && r >= rc.y && r < rc.bottom()
        }
        self.pane_close_rect.is_some_and(|rc| hit(rc, c, r))
            || self.pane_zoom_rect.is_some_and(|rc| hit(rc, c, r))
            || self.pane_title_rects.iter().any(|(_, rc)| hit(*rc, c, r))
    }

    pub fn begin_resize(&mut self, c: u16, r: u16) -> bool {
        // Pane border chrome (title, ⤢ zoom, ✕ close) always wins the click, even
        // though it sits on the seam a resize would otherwise grab — see
        // `on_pane_chrome`. This is what makes those buttons and the title
        // clickable on a stacked split, where the divider line lands on the lower
        // pane's top border row.
        if self.active_is_git() || self.active_is_orch() || self.on_pane_chrome(c, r) {
            return false;
        }
        // A cell inside a pane's *content* belongs to the pane, never to the
        // divider. `RESIZE_GRAB_TOL` makes the grab band ±2 cells wide so the
        // seam is comfortable to hit, but the gap between panes is only one
        // column — so without this the band reaches ~2 columns into each
        // neighbour and swallows clicks meant for the terminal (and starts a
        // resize the user never asked for, since `begin_resize` runs before
        // selection and mouse-forwarding). Content-exclusion also clips the band
        // back to the seam itself (borders + gap), so the tolerance only ever
        // helps you *hit* the seam, never bleeds a resize into a pane; `Ctrl`+drag
        // (`begin_resize_nearest`) stays the deliberate grab-from-anywhere path.
        if self.pane_content_at(c, r).is_some() {
            return false;
        }
        let area = self.last_pane_area;
        match self.layout().divider_at(area, c, r, RESIZE_GRAB_TOL) {
            Some(d) => {
                self.resize_drag = Some(ResizeDrag {
                    path: d.path,
                    axis: d.axis,
                });
                true
            }
            None => false,
        }
    }

    /// Start a drag of the divider nearest `(c, r)` — the `Ctrl`+drag path
    /// (RESIZE-5). Skips a pane that tracks the mouse itself (a TUI agent).
    pub fn begin_resize_nearest(&mut self, c: u16, r: u16) -> bool {
        if self.active_is_git() || self.active_is_orch() || self.on_pane_chrome(c, r) {
            return false;
        }
        let over_mouse_app = self
            .pane_rects
            .iter()
            .find(|(_, rect)| c >= rect.x && c < rect.right() && r >= rect.y && r < rect.bottom())
            .and_then(|(id, _)| self.panes.get(id))
            .is_some_and(|p| p.mouse_mode().report);
        if over_mouse_app {
            return false;
        }
        let area = self.last_pane_area;
        match self.layout().nearest_divider(area, c, r) {
            Some(d) => {
                self.resize_drag = Some(ResizeDrag {
                    path: d.path,
                    axis: d.axis,
                });
                true
            }
            None => false,
        }
    }

    /// Drive the active resize from the cursor position (RESIZE-2).
    pub fn update_resize(&mut self, c: u16, r: u16) {
        let Some(drag) = self.resize_drag.as_ref() else {
            return;
        };
        let path = drag.path.clone();
        let axis = drag.axis;
        let area = self.last_pane_area;
        if let Some(rect) = self.layout().node_rect(area, &path) {
            let ratio = match axis {
                Axis::Col => c.saturating_sub(rect.x) as f32 / rect.width.max(1) as f32,
                Axis::Row => r.saturating_sub(rect.y) as f32 / rect.height.max(1) as f32,
            };
            self.layout_mut().set_ratio(area, &path, ratio);
        }
    }

    /// End an active resize drag (RESIZE-2).
    pub fn end_resize(&mut self) {
        self.resize_drag = None;
    }

    /// Recompute the divider under the cursor for the hover highlight (RESIZE-4).
    pub fn update_hover_divider(&mut self, c: u16, r: u16) {
        self.hover_divider =
            // Mirror `begin_resize`'s content-rect rule, or the divider would
            // highlight over cells that no longer grab it.
            if self.active_is_git()
                || self.active_is_orch()
                || self.on_pane_chrome(c, r)
                || self.pane_content_at(c, r).is_some()
            {
                None
            } else {
                let area = self.last_pane_area;
                self.layout().divider_at(area, c, r, RESIZE_GRAB_TOL)
            };
    }

    /// The sidebar whose draggable edge seam is at `(c, r)`, if any (docs/29).
    /// The seam `│` column always grabs; the grab band also reaches
    /// `SIDEBAR_GRAB_TOL` columns onto the **content side** — but only over cells
    /// that are *not* a mouse-tracking pane, so an agent's own edge clicks (Claude
    /// Code expanding a tool result at its left edge) still forward, and a
    /// split's border/gap stays grabbable. It never reaches into the sidebar body,
    /// where dock rows own the width, so it can't steal a workspace/agent click.
    fn sidebar_seam_at(&self, c: u16, r: u16) -> Option<Side> {
        // The seam spans the full frame visually, but only the pane lane is a
        // resize target. The tab and status rows own their cells; in particular,
        // an overflowing tab strip places its left navigation arrow directly on
        // the left seam column.
        if r < self.last_pane_area.y || r >= self.last_pane_area.bottom() {
            return None;
        }
        for (seam, side) in [(self.left_seam, Side::Left), (self.right_seam, Side::Right)] {
            let Some(seam) = seam else { continue };
            if r < seam.y || r >= seam.bottom() {
                continue;
            }
            if c == seam.x {
                return Some(side);
            }
            // Distance onto the content side (right of a left seam, left of a
            // right seam); `None` when the cursor is on the sidebar side.
            let dist = match side {
                Side::Left => c.checked_sub(seam.x),
                Side::Right => seam.x.checked_sub(c),
            };
            let Some(d) = dist else { continue };
            let over_agent = self
                .pane_content_at(c, r)
                .and_then(|(id, _)| self.panes.get(&id))
                .is_some_and(|p| p.mouse_mode().report);
            if (1..=SIDEBAR_GRAB_TOL).contains(&d) && !over_agent {
                return Some(side);
            }
        }
        None
    }

    /// Start dragging a sidebar's edge to resize it (docs/29). Returns whether a
    /// drag began, so the mouse handler can claim the press before selection.
    pub fn begin_sidebar_resize(&mut self, c: u16, r: u16) -> bool {
        match self.sidebar_seam_at(c, r) {
            Some(side) => {
                self.sidebar_resize = Some(side);
                true
            }
            None => false,
        }
    }

    /// Drive the active sidebar resize from the cursor column. Updates the width
    /// live but does **not** persist: `save_sidebars` (a disk write) runs once on
    /// release, keeping the drag off the config-write path (the perf memory).
    pub fn update_sidebar_resize(&mut self, c: u16, _r: u16) {
        let Some(side) = self.sidebar_resize else {
            return;
        };
        let main = self.last_main_area;
        // The other sidebar's current rendered width, derived from its seam, so
        // the drag can never push the panes below the 24-column content floor.
        let other = match side {
            Side::Left => self
                .right_seam
                .map_or(0, |s| main.right().saturating_sub(s.x)),
            Side::Right => self
                .left_seam
                .map_or(0, |s| s.x.saturating_sub(main.x).saturating_add(1)),
        };
        // The left seam sits on the sidebar's *last* column (width-1), so its width
        // is the cursor column plus one; the right seam is the sidebar's *first*
        // column, so its width counts back from the right edge.
        let want = match side {
            Side::Left => c.saturating_sub(main.x).saturating_add(1),
            Side::Right => main.right().saturating_sub(c),
        };
        // Clamp to the supported range and to whatever still leaves 24 columns of
        // content; `.max(MIN)` guarantees a valid clamp range on a tiny terminal.
        let cap = main
            .width
            .saturating_sub(24 + other)
            .clamp(SIDEBAR_WIDTH_MIN, SIDEBAR_WIDTH_MAX);
        self.sidebars.get_mut(side).width = want.clamp(SIDEBAR_WIDTH_MIN, cap);
    }

    /// Finish a sidebar resize and persist the new width once (docs/29).
    pub fn end_sidebar_resize(&mut self) {
        if self.sidebar_resize.take().is_some() {
            self.save_sidebars();
        }
    }

    /// Recompute which sidebar edge (if any) the cursor is over, for the hover
    /// highlight (mirrors `update_hover_divider`).
    pub fn update_hover_sidebar(&mut self, c: u16, r: u16) {
        self.hover_sidebar = self.sidebar_seam_at(c, r);
    }

    /// Enter keyboard resize mode (RESIZE-3) — a no-op with nothing to resize.
    fn enter_resize_mode(&mut self) {
        if self.active_is_git() || self.active_is_orch() || self.layout().len() < 2 {
            return;
        }
        self.mode = Mode::Resize;
        let msg = self.catalog.mode_resize_hint;
        self.show_toast(msg);
    }

    /// Tear down the per-leaf runtime state that every close path shares: the PTY
    /// pane **or** file view (docs/38), its detection status, module-pane tracking,
    /// and any bookkeeping that must not be left pointing at a dead id. Does not
    /// touch the layout/tab — the caller owns that. Centralized so a new close
    /// path can never again forget one map (e.g. leaking a `views` entry, which
    /// made a closed file un-reopenable).
    fn drop_leaf_runtime(&mut self, id: PaneId) {
        self.emit_backend_terminal_event(id, "terminal.closed", serde_json::json!({}));
        self.backend_terminal_index.retain(|_, pane| *pane != id);
        self.backend_labels.remove(&id);
        self.cancel_backend_revision_waits(id);
        self.panes.remove(&id);
        self.status.remove(&id);
        self.views.remove(&id);
        // Parked `wait.output` calls can never see new output on a dead pane, and
        // every close path (close_pane, close_tab, close_workspace) funnels
        // through here — so cancellation cannot be forgotten by a new path.
        self.cancel_output_waits(id);
        self.cancel_agent_waits(id);
        self.editor_files.remove(&id); // untrack an editor pane's file (docs/38)
        self.module_panes.remove(&id); // untrack a module pane (MOD-2)
        self.preview_views.remove(&id); // forget a closed reusable preview pane
        if self.scroll_pane == Some(id) {
            self.scroll_pane = None; // don't leave scroll mode pointing at a dead pane
        }
        if self.copy_mode.is_some_and(|copy| copy.pane == id) {
            self.copy_mode = None; // the pane is gone; there is no viewport to restore
        }
    }

    fn close_pane(&mut self, id: PaneId) {
        self.drop_leaf_runtime(id);
        // Drop any live alias for the dead pane so a name never resolves to a
        // reallocated pane id (agent_names is ephemeral by design).
        self.agent_names.retain(|_, p| *p != id);
        // Drop an agent pin for the dead pane (per-session, id-keyed).
        self.pinned_agents.remove(&id);
        // Auto-release any orchestration leases the dead pane held (ORCH-2), so a
        // crashed/closed worker can't hold file paths forever.
        let released = self.orch.release_pane_leases(id.0);
        if !released.is_empty() {
            self.orch.save();
            self.emit_event(
                "lease.released",
                serde_json::json!({ "pane": id.0.to_string(), "leases": released }),
            );
        }
        // Unbind any task claimed by the dead pane so the board stays truthful:
        // worktree-backed work stays Running (the branch persists — `s` reopens
        // it), a pure claim with no worktree goes back to the queue.
        self.orch_unbind_pane(id.0);
        self.session_dirty = true;
        if self.layout_mut().remove(id) {
            self.close_active_tab();
        }
        self.emit_event("pane.closed", serde_json::json!({"pane": id.0.to_string()}));
    }

    fn close_active_tab(&mut self) {
        let ws = &mut self.workspaces[self.active_ws];
        if ws.active_tab < ws.tabs.len() {
            ws.tabs.remove(ws.active_tab);
        }
        if ws.tabs.is_empty() {
            self.close_active_ws();
        } else if ws.active_tab >= ws.tabs.len() {
            ws.active_tab = ws.tabs.len() - 1;
        }
    }

    fn close_active_ws(&mut self) {
        if self.active_ws < self.workspaces.len() {
            let closed_workspace_id = self.workspaces[self.active_ws].id.clone();
            if self.workspaces.len() > 1 {
                let closed_root = self.workspaces[self.active_ws].cwd.clone();
                self.fail_pending_files_api_for_root(
                    &closed_root,
                    "workspace closed while FILES was loading",
                );
                self.fail_pending_diff_api_for_root(
                    &closed_root,
                    "workspace closed while DIFF was refreshing",
                );
            }
            self.clear_workspace_transients(&closed_workspace_id);
            self.workspaces.remove(self.active_ws);
        }
        if self.workspaces.is_empty() {
            self.all_workspaces_closed();
        } else if self.active_ws >= self.workspaces.len() {
            self.active_ws = self.workspaces.len() - 1;
        }
    }

    /// The last node just closed, so the **session** is over (docs/43 §3.3).
    ///
    /// This used to reset to a fresh node at `std::env::current_dir()` — the
    /// *server's* cwd, i.e. the folder it was first launched from. Closing every
    /// node therefore resurrected the folder you closed first, the window never
    /// went away, and the snapshot kept that folder so it came back after a
    /// restart too. It also made `persist::save`'s empty-snapshot branch — which
    /// exists precisely because "the user deliberately closed everything" must
    /// not resurrect anything — unreachable in server mode.
    ///
    /// Now the *window* ends and the *server* survives, which is what
    /// `server_mode` was for: clients detach, no node is recreated, and the empty
    /// snapshot clears `session.json`. `server stop` still ends the server, and a
    /// later `luvus` attaches and opens its launch folder fresh. `--local` has no
    /// server to outlive, so it quits like a normal terminal app.
    fn all_workspaces_closed(&mut self) {
        self.session_dirty = true;
        self.fail_pending_files_api("no active workspace while FILES was loading");
        self.fail_pending_diff_api("no active workspace while DIFF was refreshing");
        if self.server_mode {
            self.end_session = true;
        } else {
            self.should_quit = true;
        }
    }

    /// Close a workspace and all of its panes.
    fn close_workspace(&mut self, index: usize) {
        if index >= self.workspaces.len() {
            return;
        }
        let closed_workspace_id = self.workspaces[index].id.clone();
        if self.workspaces.len() > 1 {
            let closed_root = self.workspaces[index].cwd.clone();
            self.fail_pending_files_api_for_root(
                &closed_root,
                "workspace closed while FILES was loading",
            );
            self.fail_pending_diff_api_for_root(
                &closed_root,
                "workspace closed while DIFF was refreshing",
            );
        }
        let ids: Vec<PaneId> = self.workspaces[index]
            .tabs
            .iter()
            .flat_map(|t| t.layout.leaves())
            .collect();
        for id in ids {
            self.drop_leaf_runtime(id);
        }
        self.clear_workspace_transients(&closed_workspace_id);
        self.workspaces.remove(index);
        if self.workspaces.is_empty() {
            self.all_workspaces_closed();
        } else if self.active_ws >= self.workspaces.len() {
            self.active_ws = self.workspaces.len() - 1;
        }
        self.session_dirty = true;
        self.emit_event(
            "workspace.closed",
            serde_json::json!({"workspace": index.to_string()}),
        );
    }

    /// Dismiss deferred UI actions whose stable target is being removed.
    fn clear_workspace_transients(&mut self, workspace_id: &str) {
        if self
            .ws_rename
            .as_ref()
            .is_some_and(|rename| rename.workspace_id == workspace_id)
        {
            self.ws_rename = None;
        }
        if self.worktree_delete.as_deref() == Some(workspace_id) {
            self.worktree_delete = None;
        }
    }

    /// Close a tab and all its panes (the "X" button / prefix+X).
    fn close_tab(&mut self, index: usize) {
        let ids: Vec<PaneId> = {
            let ws = &self.workspaces[self.active_ws];
            if index >= ws.tabs.len() {
                return;
            }
            ws.tabs[index].layout.leaves()
        };
        for id in ids {
            self.drop_leaf_runtime(id);
        }
        let ws = &mut self.workspaces[self.active_ws];
        ws.tabs.remove(index);
        if ws.tabs.is_empty() {
            self.close_active_ws();
        } else if ws.active_tab >= ws.tabs.len() {
            ws.active_tab = ws.tabs.len() - 1;
        } else if ws.active_tab > index {
            ws.active_tab -= 1;
        }
        self.session_dirty = true;
        self.emit_event(
            "tab.closed",
            serde_json::json!({"tab": (index + 1).to_string()}),
        );
    }
}

fn ws_name(cwd: &std::path::Path) -> String {
    cwd.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace")
        .to_string()
}

/// Group linked worktrees under their main checkout for the WORKSPACES list.
/// `nodes[i]` is `(git common dir, is-linked-worktree)`. Returns `(index,
/// is_member)` in draw order: a root (main checkout / non-repo node) followed
/// immediately by every linked worktree that shares its common dir. Two passes
/// so a worktree nests under its main checkout when open (pass 0 seeds groups at
/// roots only), and pass 1 still emits an orphan worktree whose checkout is not.
fn group_worktrees(nodes: &[(Option<&std::path::Path>, bool)]) -> Vec<(usize, bool)> {
    let n = nodes.len();
    let mut out = Vec::with_capacity(n);
    let mut placed = vec![false; n];
    for pass in 0..2 {
        for i in 0..n {
            if placed[i] || (pass == 0 && nodes[i].1) {
                continue;
            }
            placed[i] = true;
            out.push((i, false));
            if let Some(dir) = nodes[i].0 {
                for (j, node) in nodes.iter().enumerate() {
                    if j != i && !placed[j] && node.1 && node.0 == Some(dir) {
                        placed[j] = true;
                        out.push((j, true));
                    }
                }
            }
        }
    }
    out
}

/// `~/.luvus/worktrees/<repo>/` — the folder that holds all of `repo`'s luvus
/// worktrees. Nested under the **main** worktree's name so every checkout of one
/// repo groups under a single folder (same rule `create_worktree` uses).
fn worktrees_dir_for(repo: &std::path::Path) -> PathBuf {
    let repo_name = crate::git::local::worktrees(repo)
        .ok()
        .and_then(|wts| {
            wts.into_iter()
                .find(|w| w.is_main)
                .map(|w| ws_name(&w.path))
        })
        .unwrap_or_else(|| ws_name(repo));
    persist::config_dir().join("worktrees").join(repo_name)
}

/// Worktree grouping for a workspace at `cwd` (docs/18 WT): its git common dir, if
/// `cwd` is inside a repo. Workspaces that share one group together in the sidebar.
fn worktree_membership(cwd: &std::path::Path) -> Option<crate::git::WorktreeMembership> {
    crate::git::local::common_dir(cwd).map(|common_dir| {
        // A *linked* worktree's common dir lives in the repo's main working tree,
        // so it is never inside this checkout's own folder. `common_dir` is
        // already canonical; canonicalize `cwd` too or a symlinked path (macOS
        // `/tmp` → `/private/tmp`) reads as linked when it is the main tree.
        let real = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        let linked = !common_dir.starts_with(&real);
        crate::git::WorktreeMembership { common_dir, linked }
    })
}

/// Re-spawn a saved module pane if its module is still installed + runnable;
/// returns the pane + its tracking record, or `None` to fall back to a shell.
fn restore_module_pane(
    modules: &crate::module::ModuleRegistry,
    mid: &str,
    ep: &str,
    id: PaneId,
    app_tx: &Sender<AppEvent>,
    history_budget_bytes: usize,
) -> Option<(Pane, crate::module::ModulePaneRecord)> {
    let m = modules.find(mid).filter(|m| m.is_runnable())?;
    let argv = m
        .manifest
        .panes
        .iter()
        .find(|p| p.id == ep)
        .map(|p| p.command.clone())?;
    let ctx = serde_json::json!({ "invocation_source": "restore" });
    let env = crate::module::runtime::env(
        m,
        &ctx,
        vec![("LUVUS_MODULE_ENTRYPOINT_ID".to_string(), ep.to_string())],
    );
    let pane = Pane::spawn_command(
        id,
        80,
        24,
        m.root.clone(),
        app_tx.clone(),
        &argv,
        &env,
        history_budget_bytes,
    )
    .ok()?;
    Some((
        pane,
        crate::module::ModulePaneRecord {
            module_id: mid.to_string(),
            entrypoint: ep.to_string(),
        },
    ))
}

/// The current git branch for `cwd`, if it's inside a repo. Reads `.git/HEAD`
/// directly (no subprocess) — walks up to find the repo, follows a `.git` file
/// for worktrees, and returns a short SHA when detached.
fn git_branch(cwd: &std::path::Path) -> Option<String> {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        let dot_git = d.join(".git");
        let head = if dot_git.is_dir() {
            dot_git.join("HEAD")
        } else if dot_git.is_file() {
            // Worktree/submodule: ".git" file points at the real gitdir.
            let txt = std::fs::read_to_string(&dot_git).ok()?;
            let rel = txt.strip_prefix("gitdir:")?.trim();
            let gitdir = d.join(rel);
            gitdir.join("HEAD")
        } else {
            dir = d.parent();
            continue;
        };
        let content = std::fs::read_to_string(head).ok()?;
        let content = content.trim();
        return Some(match content.strip_prefix("ref: refs/heads/") {
            Some(branch) => branch.to_string(),
            None => content.chars().take(7).collect(), // detached HEAD → short SHA
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    use crate::persist::TEST_ENV_LOCK as ENV_GUARD;

    /// The new-pane cwd resolver hands back the chain of directories that still
    /// exist, in preference order, and never a directory it has already found
    /// missing. Regression for `spawn_cwd`'s old `unwrap_or(root)`, which
    /// returned the workspace root even after it failed the existence check.
    #[test]
    fn spawn_cwds_skips_missing_dirs_and_anchors_on_home() {
        let _env = crate::persist::test_env("spawn-cwds");
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();

        let home = crate::platform::home_dir().expect("the test host has a home directory");
        let missing = std::env::temp_dir().join(format!("luvus-missing-{}", std::process::id()));

        // Both the workspace root and the focused pane's live cwd point at a
        // directory that no longer exists.
        app.workspaces[app.active_ws].cwd = missing.clone();
        let focus = app.layout().focus;
        app.panes
            .get_mut(&focus)
            .expect("the first pane exists")
            .cwd = missing.clone();

        let chain = app.spawn_cwds();
        assert!(
            !chain.contains(&missing),
            "a deleted directory is never handed back: {chain:?}"
        );
        assert!(
            chain.iter().all(|dir| dir.is_dir()),
            "every candidate in the chain still exists: {chain:?}"
        );
        assert_eq!(
            chain.last(),
            Some(&home),
            "the chain anchors on $HOME when nothing nearer survives: {chain:?}"
        );

        // With the opt-out set, an existing workspace root leads the chain and
        // the focused pane's cwd is not consulted.
        let root = std::env::temp_dir();
        app.config.layout.new_pane_to_workspace_root = true;
        app.workspaces[app.active_ws].cwd = root.clone();
        let chain = app.spawn_cwds();
        assert_eq!(
            chain.first(),
            Some(&root),
            "root-first when the opt-out is on: {chain:?}"
        );
    }

    /// A worktree groups directly under the node it branched from, wherever that
    /// parent sits in the list — not at the worktree's raw creation position.
    #[test]
    fn worktrees_group_under_their_parent() {
        use std::path::Path;
        let sudos = Path::new("/repo/sudos/.git");
        let agave = Path::new("/repo/agave/.git");
        // Creation order: sudos (main), agave (main), a non-git node, then a
        // worktree of sudos added last — which is where the bug put it at the end.
        let nodes = vec![
            (Some(sudos), false), // 0 sudos main
            (Some(agave), false), // 1 agave main
            (None, false),        // 2 videoscript (not a repo)
            (Some(sudos), true),  // 3 test — a linked worktree of sudos
        ];
        // The worktree (3) nests right under sudos (0), not after videoscript.
        assert_eq!(
            group_worktrees(&nodes),
            vec![(0, false), (3, true), (1, false), (2, false)]
        );
    }

    /// Several worktrees of one repo stack under it, and a worktree whose main
    /// checkout is not open still shows (as its own root, second pass).
    #[test]
    fn multiple_worktrees_and_an_orphan() {
        use std::path::Path;
        let a = Path::new("/r/a/.git");
        let b = Path::new("/r/b/.git");
        let nodes = vec![
            (Some(b), true),  // 0 orphan worktree of b (main b not open)
            (Some(a), false), // 1 main a
            (Some(a), true),  // 2 worktree a1
            (Some(a), true),  // 3 worktree a2
        ];
        assert_eq!(
            group_worktrees(&nodes),
            vec![(1, false), (2, true), (3, true), (0, false)]
        );
    }

    /// A plain workspace (not a linked worktree) never offers "Delete worktree" —
    /// only worktree nodes can be deleted; a workspace can only be closed.
    #[test]
    fn delete_worktree_absent_for_a_plain_workspace() {
        let _env = crate::persist::test_env("wt-del-menu");
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        // `App::new` uses the test process's cwd. Test runs may themselves be
        // launched from a linked Git worktree, so explicitly construct the plain
        // workspace this test is about instead of depending on the checkout.
        app.workspaces[0].worktree = None;
        let items = app.ws_menu_items(0);
        assert!(
            !items.contains(&WsMenuItem::DeleteWorktree),
            "a non-worktree node has no Delete worktree action"
        );
        assert!(items.contains(&WsMenuItem::Close), "but it can be closed");
    }

    /// The worktree-delete confirm cancels on any non-confirm key, and confirming
    /// on a non-worktree node is a guarded no-op (never deletes a plain workspace).
    #[test]
    fn worktree_delete_confirm_cancels_and_guards() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let _env = crate::persist::test_env("wt-del-confirm");
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        // See `delete_worktree_absent_for_a_plain_workspace`: never let this
        // guard-path test act on the worktree that happens to run the suite.
        app.workspaces[0].worktree = None;

        // A non-confirm key dismisses the modal.
        app.worktree_delete = Some(app.workspaces[0].id.clone());
        app.worktree_delete_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(app.worktree_delete.is_none(), "n cancels");

        // Confirming on the default node (a main checkout, not a linked worktree)
        // removes nothing — the guard refuses to delete a plain workspace.
        let before = app.workspaces.len();
        app.worktree_delete = Some(app.workspaces[0].id.clone());
        app.worktree_delete_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(app.worktree_delete.is_none());
        assert_eq!(
            app.workspaces.len(),
            before,
            "a plain workspace is never deleted"
        );
    }

    fn key(c: char, m: KeyModifiers) -> AppEvent {
        AppEvent::Key(KeyEvent::new(KeyCode::Char(c), m))
    }

    #[test]
    fn prefix_chord_variants() {
        let _env = crate::persist::test_env("prefix-chord-variants");
        // Ctrl+Space arrives in different forms across terminals/OSes; each must
        // enter prefix mode and the next key (here `v`) must then split.
        let chords = [
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL), // modern Unix
            KeyEvent::new(KeyCode::Char('@'), KeyModifiers::CONTROL), // Ctrl+@ == NUL
            KeyEvent::new(KeyCode::Null, KeyModifiers::NONE),         // bare NUL byte
        ];
        for chord in chords {
            let (tx, _rx) = std::sync::mpsc::channel();
            let mut app = App::new(80, 24, tx).unwrap();
            app.handle_event(AppEvent::Key(chord));
            assert_eq!(
                app.mode,
                Mode::Prefix,
                "chord {:?} should arm the prefix",
                chord.code
            );
            app.handle_event(key('v', KeyModifiers::NONE));
            assert_eq!(
                app.layout().len(),
                2,
                "prefix+v should split after {:?}",
                chord.code
            );
        }
    }

    #[test]
    fn plain_keystroke_does_not_mark_the_ui_dirty() {
        // Typing into a pane must NOT trigger a luvus redraw — the character goes to
        // the shell, whose echo arrives as a separate PtyData event that repaints.
        // Rendering on the keystroke too would double the frame rate while typing.
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();

        assert!(
            !app.handle_event(key('x', KeyModifiers::NONE)),
            "a plain keystroke forwarded to the pane must not be dirty"
        );
        // The pane's echo of that character is what actually changes the screen.
        let id = app.layout().focus;
        assert!(
            app.handle_event(AppEvent::PtyData(id)),
            "pane output must mark the frame dirty"
        );
        // The prefix chord DOES change the UI (status bar shows PREFIX).
        assert!(
            app.handle_event(key(' ', KeyModifiers::CONTROL)),
            "entering prefix mode must repaint"
        );
    }

    #[test]
    fn paste_fills_an_open_text_input_modal_not_the_pane() {
        // Regression: pasting (e.g. an nsec) into a Settings string field leaked
        // to the focused pane instead of the field. A paste while a text-input
        // modal is open must fill that modal.
        let _env = crate::persist::test_env("paste-modal");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.module_setting_edit = Some(crate::app::ModuleSettingEdit {
            module_id: "example.buzz".into(),
            key: "nsec".into(),
            title: "Private key".into(),
            buffer: String::new(),
            secret: true,
        });
        // A paste carrying a newline (multi-line clipboard) — the field is
        // single-line, so control chars are dropped.
        app.handle_event(AppEvent::Paste("nsec1abcdef\n".into()));
        assert_eq!(
            app.module_setting_edit.as_ref().map(|e| e.buffer.as_str()),
            Some("nsec1abcdef"),
            "the paste filled the setting field, newline stripped"
        );
    }

    #[test]
    fn session_roundtrip() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        // prefix + v → split into two panes.
        app.handle_event(key(' ', KeyModifiers::CONTROL));
        app.handle_event(key('v', KeyModifiers::NONE));
        assert_eq!(app.layout().len(), 2);
        app.rename_workspace(0, "Luvus website").unwrap();
        app.set_workspace_pinned(0, true).unwrap();

        let json = serde_json::to_string(&persist::snapshot(&app)).unwrap();
        let snap: SessionSnapshot = serde_json::from_str(&json).unwrap();

        let (tx2, _rx2) = mpsc::channel();
        let restored = App::from_snapshot(snap, tx2).expect("restore");
        assert_eq!(restored.workspaces.len(), 1);
        assert_eq!(restored.layout().len(), 2);
        assert_eq!(restored.workspaces[0].name, "Luvus website");
        assert!(restored.workspaces[0].pinned);
    }

    // A saved pane whose cwd no longer exists (deleted project dir) must not
    // cost the user the whole session: the pane falls back to the workspace
    // dir / home and everything else restores intact.
    #[test]
    fn restore_survives_a_deleted_pane_cwd() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.handle_event(key(' ', KeyModifiers::CONTROL));
        app.handle_event(key('v', KeyModifiers::NONE));
        assert_eq!(app.layout().len(), 2);

        let mut snap = persist::snapshot(&app);
        // Simulate one pane's project dir vanishing between save and restore.
        snap.workspaces[0].tabs[0].panes[0].1.cwd =
            std::path::PathBuf::from("/nonexistent/deleted-project-xyz");

        let (tx2, _rx2) = mpsc::channel();
        let restored = App::from_snapshot(snap, tx2).expect("session survives a missing pane cwd");
        assert_eq!(restored.workspaces.len(), 1, "workspace kept");
        assert_eq!(
            restored.layout().len(),
            2,
            "both panes restored (one fell back)"
        );
        // Every restored pane spawned somewhere real.
        assert_eq!(restored.panes.len(), 2);
    }

    #[test]
    fn splits_both_directions() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let area = Rect::new(0, 0, 80, 24);

        // `v` → side-by-side (vertical divider): same y, different x.
        app.handle_event(key(' ', KeyModifiers::CONTROL));
        app.handle_event(key('v', KeyModifiers::NONE));
        let r = app.layout().panes(area);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].rect.y, r[1].rect.y);
        assert_ne!(r[0].rect.x, r[1].rect.x);

        // `s` → stacked (horizontal divider): a pair sharing x but different y.
        app.handle_event(key(' ', KeyModifiers::CONTROL));
        app.handle_event(key('s', KeyModifiers::NONE));
        let r = app.layout().panes(area);
        assert_eq!(r.len(), 3);
        let stacked = r.iter().any(|a| {
            r.iter()
                .any(|b| a.rect.x == b.rect.x && a.rect.y != b.rect.y)
        });
        assert!(stacked, "horizontal-divider split not produced by `s`");
    }

    #[test]
    fn border_only_when_split() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        // Isolate `$LUVUS_HOME` so this renders the *default* layout. Without it
        // the test read the developer's real `~/.luvus` config, so a user who had
        // (say) a right sidebar mounted saw its chrome counted as pane borders —
        // failing alone but passing in a full run, purely because some earlier
        // test's `test_env` had already redirected the global env var.
        let _env = crate::persist::test_env("border-split");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        // Borders use ratatui's native box-drawing glyphs, so count cells
        // carrying one of them in the pane area (right of the sidebar).
        let border_cells = |app: &mut App| -> usize {
            let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
            term.draw(|f| crate::ui::render(f, app)).unwrap();
            let buf = term.backend().buffer().clone();
            let px = app.last_pane_area.x;
            buf.content()
                .iter()
                .enumerate()
                .filter(|(i, c)| {
                    let x = (*i as u16) % 100;
                    x >= px && matches!(c.symbol(), "│" | "─" | "┌" | "┐" | "└" | "┘")
                })
                .count()
        };
        // A lone pane: no border.
        assert_eq!(
            border_cells(&mut app),
            0,
            "single pane should have no border"
        );
        // After a split: the panes are framed.
        app.handle_event(key(' ', KeyModifiers::CONTROL));
        app.handle_event(key('v', KeyModifiers::NONE));
        assert!(border_cells(&mut app) > 0, "split panes should be bordered");
    }

    #[test]
    fn click_focuses_pane() {
        use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.handle_event(key(' ', KeyModifiers::CONTROL));
        app.handle_event(key('v', KeyModifiers::NONE)); // split → 2 panes
        let leaves = app.layout().leaves();
        let (a, b) = (leaves[0], leaves[1]);
        assert_eq!(app.layout().focus, b); // new pane focused after split

        // Simulate the render having recorded pane hitboxes.
        app.pane_rects = vec![(a, Rect::new(0, 0, 10, 10)), (b, Rect::new(10, 0, 10, 10))];
        app.handle_event(AppEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row: 3,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(app.layout().focus, a); // click in pane a focuses it
    }

    #[test]
    fn text_modal_swallowing_a_click_does_not_close_bar_overflow() {
        use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

        let _env = crate::persist::test_env("modal-before-bar");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.worktree_prompt = Some(String::new());
        app.bar.overflow = Some(crate::bar::OverflowPopup {
            region: crate::bar::BarRegion::BottomRight,
            keys: vec![crate::bar::CORE_RUNTIME.to_string()],
            rect: Rect::new(60, 20, 10, 3),
        });
        app.handle_event(AppEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        }));

        assert!(app.worktree_prompt.is_some());
        assert!(
            app.bar.overflow.is_some(),
            "the modal owns the click before hidden bar hit targets"
        );
    }

    #[test]
    fn close_tab_removes_it_and_its_panes() {
        let _env = crate::persist::test_env("close-tab");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.handle_event(key(' ', KeyModifiers::CONTROL));
        app.handle_event(key('c', KeyModifiers::NONE)); // new tab (+ its pane)
        assert_eq!(app.ws().tabs.len(), 2);
        let before = app.panes.len();

        app.handle_event(key(' ', KeyModifiers::CONTROL));
        app.handle_event(key('X', KeyModifiers::NONE)); // close the tab's only pane → tab drops
        assert_eq!(app.ws().tabs.len(), 1);
        assert!(app.panes.len() < before);
    }

    #[test]
    fn picker_w_creates_a_worktree_only_on_a_repo() {
        let mk = |path: &str, is_repo: bool| crate::app::FolderPicker {
            path: std::path::PathBuf::from(path),
            entries: Vec::new(),
            cursor: 0,
            creating: None,
            going_to: None,
            error: None,
            is_repo,
            show_hidden: false,
        };

        // On a git repo: `w` closes the picker and opens the branch prompt,
        // targeting the browsed folder.
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.picker = Some(mk("/tmp/some-repo", true));
        app.handle_picker_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
        assert!(app.picker.is_none(), "picker closes");
        assert!(app.worktree_prompt.is_some(), "branch prompt opens");
        assert_eq!(
            app.worktree_repo,
            Some(std::path::PathBuf::from("/tmp/some-repo"))
        );

        // On a plain folder: `w` is inert.
        let (tx2, _rx2) = std::sync::mpsc::channel();
        let mut app2 = App::new(80, 24, tx2).unwrap();
        app2.picker = Some(mk("/tmp/plain", false));
        app2.handle_picker_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
        assert!(app2.picker.is_some(), "non-repo: picker stays open");
        assert!(app2.worktree_prompt.is_none(), "non-repo: no prompt");
    }

    #[test]
    fn worktree_prompt_surfaces_errors_instead_of_silently_failing() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        // A non-repo target → create_worktree fails at the is_repo check.
        app.worktree_repo = Some(std::path::PathBuf::from("/definitely/not/a/repo"));
        app.worktree_prompt = Some("feature".to_string());

        app.handle_worktree_prompt_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            app.worktree_error.is_some(),
            "the failure is shown, not swallowed"
        );
        assert!(
            app.worktree_prompt.is_some(),
            "prompt stays open so you can retry"
        );
        assert!(
            app.worktree_repo.is_some(),
            "target repo is retained for the retry"
        );

        // Editing the branch clears the stale error.
        app.handle_worktree_prompt_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(app.worktree_error.is_none(), "editing clears the error");

        // Esc tears the whole prompt down.
        app.handle_worktree_prompt_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.worktree_prompt.is_none() && app.worktree_repo.is_none());
    }

    #[test]
    fn selection_keeps_the_drag_left_edge_between_lines() {
        // Content rect at (x=2, y=1), 10 wide × 5 tall.
        let content = Rect::new(2, 1, 10, 5);
        let sel = Selection {
            pane: PaneId(1),
            content,
            anchor: (4, 1),
            cursor: (6, 3),
            retained: None,
            scrolled: false,
            dragging: false,
        };
        // First row: from the anchor column to the right edge.
        assert!(sel.contains(4, 1));
        assert!(sel.contains(11, 1)); // last column (right() == 12)
        assert!(!sel.contains(3, 1)); // before the anchor
                                      // Middle row: it keeps the drag's left edge instead of
                                      // expanding into the pane's left margin.
        assert!(!sel.contains(2, 2));
        assert!(sel.contains(4, 2) && sel.contains(11, 2));
        // Last row: up to the cursor column.
        assert!(sel.contains(6, 3));
        assert!(!sel.contains(7, 3)); // past the cursor
                                      // Outside the row range / pane.
        assert!(!sel.contains(5, 0) && !sel.contains(5, 4) && !sel.contains(99, 2));
        // Dragging up-left selects the same range (anchor/cursor order-independent).
        let rev = Selection {
            anchor: (6, 3),
            cursor: (4, 1),
            ..sel
        };
        assert!(rev.contains(11, 1) && rev.contains(6, 3) && !rev.contains(7, 3));
    }

    #[test]
    fn toast_shows_then_expires() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        assert!(app.toast.is_none());
        app.show_toast("Copied");
        assert!(app.toast.is_some());
        // Not expired yet → no change.
        assert!(!app.tick_toast(Instant::now()));
        assert!(app.toast.is_some());
        // Past the expiry → cleared, returns true so the loop redraws once.
        assert!(app.tick_toast(Instant::now() + Duration::from_secs(5)));
        assert!(app.toast.is_none());
    }

    /// The reported bug (docs/43 §3.3): open projectA then projectB, close
    /// projectA, then close projectB — and **projectA came back** while the app
    /// refused to close. `all_workspaces_closed` reset to a fresh node at
    /// `std::env::current_dir()`, which in the server is the folder it was
    /// launched from, i.e. projectA.
    ///
    /// The server still outlives its windows (that part was always intended);
    /// what ends now is the *session*.
    #[test]
    fn closing_the_last_node_ends_the_session_without_resurrecting_one() {
        let _env = crate::persist::test_env("server-outlives");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.server_mode = true;

        // A second node, so this mirrors the report (close one, then the other)
        // rather than the single-node case.
        let dir = std::env::temp_dir().join("luvus-lastnode-4b1c9f");
        std::fs::create_dir_all(&dir).expect("temp dir");
        assert!(app.create_workspace_at(dir.clone()));
        assert_eq!(app.workspaces.len(), 2);

        // Close them both, ending with the last one.
        app.close_workspace(0);
        assert_eq!(app.workspaces.len(), 1, "the other node stays open");
        app.close_workspace(0);

        assert!(
            app.workspaces.is_empty(),
            "no node is resurrected — least of all the server's launch folder"
        );
        assert!(
            !app.should_quit,
            "the server itself still outlives its windows; `server stop` ends it"
        );
        assert!(
            app.end_session,
            "every client is told to detach, so the window actually closes"
        );

        // The server keeps ticking after the session ends, with no clients
        // attached. `detect_tick` reaches `layout()`, so an unguarded empty
        // session panics the whole server here — which is exactly what happened,
        // and what no assertion above would have caught.
        for _ in 0..3 {
            assert!(
                !app.detect_tick(Instant::now()),
                "an empty session has nothing to detect"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With no node open, the UHP must answer with an error rather than
    /// indexing an empty `workspaces` and taking the server down. `handle_api`
    /// already guards this centrally; the session can now *stay* empty rather
    /// than being a brief pre-quit window, so this pins that guard in place.
    #[test]
    fn node_scoped_api_reports_no_session_instead_of_panicking() {
        let _env = crate::persist::test_env("api-no-node");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.server_mode = true;
        app.close_workspace(0);
        assert!(app.workspaces.is_empty());

        let call = |app: &mut App, method: &str, params: serde_json::Value| {
            let (reply, _r) = std::sync::mpsc::channel();
            let resp = app.handle_api(&crate::ipc::api::ApiRequest {
                id: "1".into(),
                method: method.into(),
                params,
                reply,
            });
            serde_json::from_str::<serde_json::Value>(&resp).unwrap()
        };

        for method in [
            "workspace.rename",
            "workspace.pin",
            "tab.list",
            "tab.new",
            "tab.move",
            "tab.swap",
            "tab.close",
            "tab.rename",
            "pane.move",
        ] {
            let res = call(&mut app, method, serde_json::json!({}));
            assert_eq!(
                res.pointer("/error/code").and_then(|v| v.as_str()),
                Some("no_session"),
                "{method} must report the empty session, not panic: {res}"
            );
        }

        // ...but the methods that *open* a node have to get through, or an empty
        // server is a brick only `server stop` can clear. This is the recovery
        // path a client takes when it attaches after a session ended, and the
        // blanket guard used to swallow it.
        let res = call(&mut app, "workspace.list", serde_json::json!({}));
        assert!(
            res.get("error").is_none(),
            "listing nodes with none open is not an error: {res}"
        );

        let dir = std::env::temp_dir().join("luvus-recover-4b1c9f");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let res = call(
            &mut app,
            "workspace.open",
            serde_json::json!({ "path": dir.display().to_string() }),
        );
        assert!(
            res.get("error").is_none(),
            "an empty server must still be able to open a folder: {res}"
        );
        assert_eq!(app.workspaces.len(), 1, "the session is back");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same empty session must render. A client can still be attached for the
    /// frame or two before it detaches, and `luvus attach` / `--remote` can attach
    /// before any folder is opened — every draw fn below `render` assumes a node.
    #[test]
    fn an_empty_session_still_renders() {
        let _env = crate::persist::test_env("render-no-node");
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.server_mode = true;
        app.close_workspace(0);
        assert!(app.workspaces.is_empty());

        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let text: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            text.contains("no folders open"),
            "the empty session says so instead of drawing a broken frame"
        );
    }

    #[test]
    fn closing_last_pane_quits_and_ignores_further_events() {
        let _env = crate::persist::test_env("close-last-pane");
        // Closing the last pane empties `workspaces` and sets `should_quit`; the
        // server loop drains the rest of the event batch before checking that
        // flag, so late events must be no-ops, not panics on an empty Vec.
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let id = app.layout().focus;
        app.handle_event(AppEvent::PtyExit(id)); // the only pane's shell exits
        assert!(app.should_quit, "closing the last pane quits the session");
        assert!(app.workspaces.is_empty());
        // Late events in the same batch must not panic.
        app.handle_event(key(' ', KeyModifiers::CONTROL));
        app.handle_event(key('c', KeyModifiers::NONE));
        app.handle_event(AppEvent::PtyExit(id));
    }

    #[test]
    fn agents_list_is_global() {
        let _env = crate::persist::test_env("agents-global");
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.handle_event(key(' ', KeyModifiers::CONTROL));
        app.handle_event(key('c', KeyModifiers::NONE)); // 2nd tab + its pane
        let ids: Vec<PaneId> = app.panes.keys().copied().collect();
        app.status.get_mut(&ids[0]).unwrap().agent = "claude".into();
        app.status.get_mut(&ids[1]).unwrap().agent = "codex".into();

        let mut term = Terminal::new(TestBackend::new(110, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let text: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        // Both agents show even though only one tab is active.
        assert!(text.contains("claude"), "claude agent missing");
        assert!(
            text.contains("codex"),
            "second-tab agent missing from global list"
        );
    }

    #[test]
    fn tabbar_scrolls_when_full() {
        let _env = crate::persist::test_env("tabbar-full");
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        // This test covers the desktop tab strip. Automatic mobile presentation
        // intentionally replaces it at this width.
        app.config.layout.mobile_width = 0;
        // Add enough tabs to overflow a narrow tab bar.
        for _ in 0..4 {
            app.handle_event(key(' ', KeyModifiers::CONTROL));
            app.handle_event(key('c', KeyModifiers::NONE));
        }
        let mut term = Terminal::new(TestBackend::new(50, 16)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let text: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        // Overflowing tabs render scroll arrows, and the active tab stays visible.
        assert!(
            text.contains('‹') || text.contains('›'),
            "scroll arrows missing when tabs overflow"
        );
        assert!(
            text.contains('5'),
            "active tab (5) not visible after scroll"
        );
    }

    #[test]
    fn agent_session_persists_and_resumes() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let focus = app.layout().focus;

        let (reply, _r) = mpsc::channel();
        app.handle_api(&ApiRequest {
            id: "1".into(),
            method: "pane.report_session".into(),
            params: json!({"pane": focus.0.to_string(), "agent": "claude", "session_id": "abc-123"}),
            reply,
        });
        assert!(app.status.get(&focus).unwrap().agent_session.is_some());

        let json = serde_json::to_string(&persist::snapshot(&app)).unwrap();
        let snap: SessionSnapshot = serde_json::from_str(&json).unwrap();
        let (tx2, _rx2) = mpsc::channel();
        let restored = App::from_snapshot(snap, tx2).expect("restore");
        let rid = restored.layout().focus;
        let sess = restored
            .status
            .get(&rid)
            .unwrap()
            .agent_session
            .as_ref()
            .unwrap();
        assert_eq!(sess.agent, "claude");
        assert_eq!(sess.session_id, "abc-123");
    }

    #[test]
    fn exited_agent_becomes_shell_and_is_not_resumed() {
        let _env = crate::persist::test_env("agent-exit-shell");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let id = app.layout().focus;
        let root = app
            .panes
            .get(&id)
            .unwrap()
            .child_pid
            .load(std::sync::atomic::Ordering::SeqCst);
        assert_ne!(root, 0, "the fresh pane has a spawned child");
        let shell = app.panes.get(&id).unwrap().command.clone();
        {
            let st = app.status.get_mut(&id).unwrap();
            st.agent = "claude".into();
            st.agent_session = Some(AgentSession {
                agent: "claude".into(),
                session_id: "finished-session".into(),
            });
        }
        let scan = |commands: &[&str]| {
            Some(HashMap::from([(
                root,
                commands.iter().map(|s| s.to_string()).collect(),
            )]))
        };

        // One shell-only observation is not enough: an agent may be starting or
        // re-execing. Seeing it again resets the exit candidate, even when a
        // different recognised agent appears earlier in the same process tree.
        assert!(
            !app.apply_proc_scan(scan(&[&shell])),
            "the first missing scan only updates the process cache"
        );
        assert!(app.status.get(&id).unwrap().agent_session.is_some());
        assert_eq!(app.status.get(&id).unwrap().agent_absent_scans, 1);
        assert!(
            !app.apply_proc_scan(scan(&[&shell, "codex", "claude"])),
            "seeing the bound agent again does not change visible lifecycle state"
        );
        assert!(app.status.get(&id).unwrap().agent_session.is_some());
        assert_eq!(app.status.get(&id).unwrap().agent_absent_scans, 0);

        // Two confirmed scans back at the shell clear the resume binding and
        // dirty persistence. A detach may now leave this pane alive as a shell;
        // a later server restart must not relaunch the exited agent.
        app.session_dirty = false;
        assert!(!app.handle_event(AppEvent::ProcScanned(scan(&[&shell]))));
        assert!(app.status.get(&id).unwrap().agent_session.is_some());
        assert!(
            app.handle_event(AppEvent::ProcScanned(scan(&[&shell]))),
            "the confirmed exit dirties the sidebar through the event path"
        );
        let st = app.status.get(&id).unwrap();
        assert!(st.agent_session.is_none());
        assert_eq!(st.agent, shell);
        assert!(app.session_dirty);

        let pane = persist::snapshot(&app)
            .workspaces
            .into_iter()
            .flat_map(|ws| ws.tabs)
            .flat_map(|tab| tab.panes)
            .find(|(raw, _)| *raw == id.0)
            .map(|(_, pane)| pane)
            .unwrap();
        assert_eq!(
            pane.agent_session, None,
            "the exited agent is not resumable"
        );
    }

    /// A pane's live name (`pane name`) survives a restart: it is re-attached to
    /// the pane's freshly allocated id on restore, so the sidebar/title keep it.
    #[test]
    fn pane_name_survives_a_restart() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let focus = app.layout().focus;
        app.set_agent_name(focus, Some("backend"));
        assert_eq!(app.agent_name_for(focus), Some("backend"));

        let json = serde_json::to_string(&persist::snapshot(&app)).unwrap();
        let snap: SessionSnapshot = serde_json::from_str(&json).unwrap();
        let (tx2, _rx2) = std::sync::mpsc::channel();
        let restored = App::from_snapshot(snap, tx2).expect("restore");

        // The pane came back under a new id, but its name followed it.
        let rid = restored.layout().focus;
        assert_eq!(restored.agent_name_for(rid), Some("backend"));
    }

    #[test]
    fn ws_menu_pins_and_unpins_and_floats_to_top() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        // A fresh node is unpinned, so the menu offers "Pin".
        assert!(app.ws_menu_items(0).contains(&WsMenuItem::Pin));
        assert!(!app.ws_menu_items(0).contains(&WsMenuItem::Unpin));

        // Pin it via the right-click action.
        app.open_ws_menu(0, 0, 0);
        app.ws_menu_action(WsMenuItem::Pin);
        assert!(app.workspaces[0].pinned, "pinned after Pin");
        // Now the menu offers "Unpin" instead.
        assert!(app.ws_menu_items(0).contains(&WsMenuItem::Unpin));
        // A pinned node sorts to the front of the display order.
        assert_eq!(
            app.workspace_display_order().first().map(|&(i, _)| i),
            Some(0)
        );

        // Unpin restores it.
        app.open_ws_menu(0, 0, 0);
        app.ws_menu_action(WsMenuItem::Unpin);
        assert!(!app.workspaces[0].pinned, "unpinned after Unpin");

        // A linked worktree remains nested under its parent. Pinning the child
        // floats that complete group instead of recording a no-op pin.
        let pane = app.layout().focus;
        let common_dir = PathBuf::from("/tmp/luvus-group/.git");
        app.workspaces.push(Workspace {
            id: crate::ids::public_id("workspace"),
            name: "parent".into(),
            cwd: PathBuf::from("/tmp/luvus-group"),
            branch: None,
            git_ahead_behind: None,
            worktree: Some(crate::git::WorktreeMembership {
                common_dir: common_dir.clone(),
                linked: false,
            }),
            tabs: vec![Tab::panes(TileLayout::new(pane))],
            active_tab: 0,
            pinned: false,
        });
        app.workspaces.push(Workspace {
            id: crate::ids::public_id("workspace"),
            name: "child".into(),
            cwd: PathBuf::from("/tmp/luvus-group-child"),
            branch: None,
            git_ahead_behind: None,
            worktree: Some(crate::git::WorktreeMembership {
                common_dir,
                linked: true,
            }),
            tabs: vec![Tab::panes(TileLayout::new(pane))],
            active_tab: 0,
            pinned: false,
        });
        app.set_workspace_pinned(2, true).unwrap();
        assert_eq!(
            app.workspace_display_order(),
            vec![(1, false), (2, true), (0, false)]
        );
    }

    #[test]
    fn open_workspace_menu_reuses_snapshotted_repo_capability() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        // A deliberately missing path would fail a fresh `git rev-parse`. The
        // open menu must still use its captured result instead of launching Git
        // again during rendering.
        app.workspaces[0].cwd = std::path::PathBuf::from("luvus-missing-menu-repo");
        app.ws_menu = Some(WsMenu {
            workspace_id: app.workspaces[0].id.clone(),
            is_repo: true,
            anchor: (0, 0),
            items: Vec::new(),
            module_actions: Vec::new(),
        });

        assert!(app.ws_menu_items(0).contains(&WsMenuItem::OpenGit));
        assert!(app.ws_menu_items(0).contains(&WsMenuItem::NewWorktree));
    }

    #[test]
    fn workspace_menu_follows_target_identity_after_an_index_shift() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let target_id = crate::ids::public_id("workspace");
        app.workspaces.push(Workspace {
            id: target_id.clone(),
            name: "target".into(),
            cwd: app.workspaces[0].cwd.clone(),
            branch: None,
            git_ahead_behind: None,
            worktree: None,
            tabs: vec![Tab::panes(TileLayout::new(PaneId::alloc()))],
            active_tab: 0,
            pinned: false,
        });

        app.open_ws_menu(1, 0, 0);
        app.close_workspace(0);
        assert_eq!(app.ws_menu_target_index(), Some(0));

        app.ws_menu_action(WsMenuItem::Rename);
        let rename = app
            .ws_rename
            .as_ref()
            .expect("rename targets surviving node");
        assert_eq!(rename.workspace_id, target_id);
    }

    #[test]
    fn workspace_rename_keeps_target_identity_while_modal_is_open() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let target_id = crate::ids::public_id("workspace");
        app.workspaces.push(Workspace {
            id: target_id.clone(),
            name: "target".into(),
            cwd: app.workspaces[0].cwd.clone(),
            branch: None,
            git_ahead_behind: None,
            worktree: None,
            tabs: vec![Tab::panes(TileLayout::new(PaneId::alloc()))],
            active_tab: 0,
            pinned: false,
        });

        app.open_ws_rename(1);
        app.close_workspace(0);
        app.ws_rename.as_mut().unwrap().buffer = "renamed".into();
        app.handle_ws_rename_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.workspaces[0].id, target_id);
        assert_eq!(app.workspaces[0].name, "renamed");
    }

    #[test]
    fn deferred_workspace_actions_abort_after_target_closes() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let survivor_id = crate::ids::public_id("workspace");
        app.workspaces.push(Workspace {
            id: survivor_id.clone(),
            name: "survivor".into(),
            cwd: app.workspaces[0].cwd.clone(),
            branch: None,
            git_ahead_behind: None,
            worktree: None,
            tabs: vec![Tab::panes(TileLayout::new(PaneId::alloc()))],
            active_tab: 0,
            pinned: false,
        });

        let removed_id = app.workspaces[0].id.clone();
        app.open_ws_rename(0);
        app.worktree_delete = Some(removed_id);
        app.close_workspace(0);

        assert_eq!(app.workspaces.len(), 1);
        assert_eq!(app.workspaces[0].id, survivor_id);
        assert_eq!(app.workspaces[0].name, "survivor");
        assert!(app.ws_rename.is_none());
        assert!(app.worktree_delete.is_none());
    }

    #[test]
    fn workspace_menu_dismisses_when_its_target_was_closed() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.open_ws_menu(0, 0, 0);
        app.close_workspace(0);

        assert_eq!(app.ws_menu_target_index(), None);
        app.ws_menu_action(WsMenuItem::Rename);
        assert!(app.ws_menu.is_none());
        assert!(app.ws_rename.is_none());
    }

    #[test]
    fn agent_menu_pins_and_unpins_a_live_agent() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let id = app.layout().focus;
        // A live agent's menu offers Pin (below Rename/Close); a session cannot pin.
        assert!(app
            .agent_menu_items(AgentTarget::Live(id))
            .contains(&AgentMenuItem::Pin));
        assert!(!app
            .agent_menu_items(AgentTarget::Session(0))
            .contains(&AgentMenuItem::Pin));

        app.open_agent_menu(AgentTarget::Live(id), 0, 0);
        app.agent_menu_action(AgentMenuItem::Pin);
        assert!(app.pinned_agents.contains(&id), "pinned");
        // Now the menu offers Unpin.
        assert!(app
            .agent_menu_items(AgentTarget::Live(id))
            .contains(&AgentMenuItem::Unpin));

        app.open_agent_menu(AgentTarget::Live(id), 0, 0);
        app.agent_menu_action(AgentMenuItem::Unpin);
        assert!(!app.pinned_agents.contains(&id), "unpinned");
    }

    /// The flags an agent pane was launched with (docs/62) are pulled from the
    /// captured process argv into the snapshot, and survive a JSON round-trip. An
    /// older snapshot with no such field loads as `None` (serde default), so the
    /// change is backward compatible.
    #[test]
    fn agent_launch_flags_are_captured_in_the_snapshot() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let focus = app.layout().focus;
        // Stand in for the detection scan: a claude pane started with flags.
        //
        // The session is set **explicitly**. The options are keyed off the agent in
        // `agent_session`, and without one that resolves by scanning `~/.claude`,
        // which makes the test pass or fail on whether the machine running it
        // happens to have Claude sessions on disk.
        let st = app.status.get_mut(&focus).unwrap();
        st.agent = "claude".into();
        st.agent_session = Some(AgentSession {
            agent: "claude".into(),
            session_id: "s-flags".into(),
        });
        app.proc_commands.insert(
            focus,
            vec!["claude --model opus --permission-mode bypassPermissions".into()],
        );

        let launch_of = |snap: &SessionSnapshot| -> Option<Vec<String>> {
            snap.workspaces
                .iter()
                .flat_map(|w| &w.tabs)
                .flat_map(|t| &t.panes)
                .find(|(id, _)| *id == focus.0)
                .and_then(|(_, ps)| ps.agent_launch.clone())
        };

        let snap = persist::snapshot(&app);
        assert_eq!(
            launch_of(&snap),
            Some(vec![
                "--model".into(),
                "opus".into(),
                "--permission-mode".into(),
                "bypassPermissions".into(),
            ])
        );

        // Survives serialization.
        let json = serde_json::to_string(&snap).unwrap();
        let back: SessionSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(launch_of(&back).unwrap().len(), 4);

        // A PaneSnap written before this field existed still loads (serde default
        // -> None), so old sessions are unaffected.
        let old: persist::PaneSnap =
            serde_json::from_str(r#"{"cwd":"/tmp/x","command":"sh"}"#).unwrap();
        assert_eq!(old.agent_launch, None);
    }

    /// The captured CLI options are **per pane**, not one global set (docs/62).
    ///
    /// `proc_commands` is keyed by `PaneId` and filled from each pane's own
    /// `child_pid`, so two agents launched differently must each get their own
    /// options back. A regression here would replay one pane's `--sandbox
    /// danger-full-access` into a neighbour that never asked for it.
    #[test]
    fn launch_options_are_captured_per_pane_not_shared() {
        let _env = crate::persist::test_env("per-pane-flags");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let a = app.layout().focus;
        app.split(Axis::Col);
        let b = app.layout().focus;
        assert_ne!(a, b, "two distinct panes");

        // Each pane runs its own agent, with its own hook-reported session, so the
        // options are stored against the agent that will actually be resumed.
        let st = app.status.get_mut(&a).unwrap();
        st.agent = "claude".into();
        st.agent_session = Some(AgentSession {
            agent: "claude".into(),
            session_id: "s-a".into(),
        });
        app.proc_commands
            .insert(a, vec!["claude --model opus".into()]);
        let st = app.status.get_mut(&b).unwrap();
        st.agent = "codex".into();
        st.agent_session = Some(AgentSession {
            agent: "codex".into(),
            session_id: "s-b".into(),
        });
        app.proc_commands
            .insert(b, vec!["codex --sandbox danger-full-access".into()]);

        let snap = persist::snapshot(&app);
        let launch = |id: PaneId| -> Option<Vec<String>> {
            snap.workspaces
                .iter()
                .flat_map(|w| &w.tabs)
                .flat_map(|t| &t.panes)
                .find(|(pid, _)| *pid == id.0)
                .and_then(|(_, ps)| ps.agent_launch.clone())
        };
        assert_eq!(launch(a), Some(vec!["--model".into(), "opus".into()]));
        assert_eq!(
            launch(b),
            Some(vec!["--sandbox".into(), "danger-full-access".into()])
        );

        // With the feature switched **on**, each pane's resume command carries
        // only its own agent's options. The switch turns the feature on; it never
        // pools options across agents, so codex can never be handed claude's
        // `--model opus`.
        let cmd = |id: PaneId| -> String {
            let ps = snap
                .workspaces
                .iter()
                .flat_map(|w| &w.tabs)
                .flat_map(|t| &t.panes)
                .find(|(pid, _)| *pid == id.0)
                .map(|(_, ps)| ps)
                .unwrap();
            let (agent, sid) = ps.agent_session.clone().unwrap();
            crate::agent::resume_for(&agent, &sid, ps.agent_launch.as_deref(), true).unwrap()
        };
        let (ca, cb) = (cmd(a), cmd(b));
        assert!(ca.contains("'--model' 'opus'"), "{ca}");
        assert!(!ca.contains("sandbox"), "claude has none of codex's: {ca}");
        assert!(cb.contains("'--sandbox' 'danger-full-access'"), "{cb}");
        assert!(
            !cb.contains("model") && !cb.contains("opus"),
            "codex must never run claude's model: {cb}"
        );
    }

    /// The captured options must belong to the agent being **resumed**, not to
    /// whatever detection currently sees in the pane (docs/62).
    ///
    /// The two can disagree: a hook reports a session precisely, while detection
    /// reads the screen. Keying the options off the detected name handed one
    /// agent's CLI options to another agent's resume command, e.g.
    /// `codex resume <id> --model opus` from a neighbouring claude.
    #[test]
    fn launch_options_never_cross_between_agents() {
        let _env = crate::persist::test_env("no-bleed");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let id = app.layout().focus;
        // Detection says claude; the hook reported a *codex* session.
        let st = app.status.get_mut(&id).unwrap();
        st.agent = "claude".into();
        st.agent_session = Some(AgentSession {
            agent: "codex".into(),
            session_id: "sess-1".into(),
        });
        app.proc_commands
            .insert(id, vec!["claude --model opus".into()]);

        let snap = persist::snapshot(&app);
        let ps = snap
            .workspaces
            .iter()
            .flat_map(|w| &w.tabs)
            .flat_map(|t| &t.panes)
            .find(|(p, _)| *p == id.0)
            .map(|(_, ps)| ps)
            .expect("the pane is in the snapshot");

        assert_eq!(
            ps.agent_session,
            Some(("codex".into(), "sess-1".into())),
            "codex is what gets resumed"
        );
        assert_eq!(
            ps.agent_launch, None,
            "claude's options must not ride along with codex's resume"
        );

        // And the command that would actually run carries no stray options.
        let (a, sid) = ps.agent_session.clone().unwrap();
        let cmd = crate::agent::resume_for(&a, &sid, ps.agent_launch.as_deref(), true).unwrap();
        assert!(!cmd.contains("opus"), "{cmd}");
        assert_eq!(cmd, crate::agent::resume_command(&a, &sid).unwrap());
    }

    /// Two agent panes in one folder must not both restore the *same* session.
    ///
    /// With no hook-reported id, `persist::snapshot` falls back to
    /// `agent::latest_session(agent, cwd)` — "the newest session for this agent
    /// in this folder". That key is identical for every pane sharing a cwd, and
    /// tabs are not part of it at all, so both panes recorded the same session
    /// id and restored into the same conversation: the reported "a session from
    /// another pane/tab was resumed here". It also corrupts the transcript, as
    /// two agents then append to one file. A pane with no session of its own has
    /// to come back as a plain shell instead.
    #[test]
    fn two_panes_in_one_folder_do_not_resume_the_same_session() {
        let _env = crate::persist::test_env("dup-session");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.run_cmd(crate::app::keys::Cmd::SplitRight);
        let ids = app.layout().leaves();
        assert_eq!(ids.len(), 2, "two panes in one tab");

        // Both panes share a cwd (a split inherits it), which is the whole key.
        let cwd = app.panes.get(&ids[0]).unwrap().cwd.clone();
        assert_eq!(app.panes.get(&ids[1]).unwrap().cwd, cwd, "same folder");

        // A Claude store holding exactly one session for that folder.
        let store = std::env::temp_dir().join(format!("luvus-dupsess-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&store);
        // The real encoder, so a test can never drift from what luvus looks up.
        let proj = crate::agent::claude_project_dir(&store, &cwd);
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("only-session.jsonl"), "{}").unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", &store);

        // Neither pane has a hook-reported session, so both fall back to the guess.
        for id in &ids {
            let st = app.status.get_mut(id).unwrap();
            st.agent = "claude".into();
            st.agent_session = None;
        }

        let snap = crate::persist::snapshot(&app);
        let sessions: Vec<Option<String>> = snap.workspaces[0].tabs[0]
            .panes
            .iter()
            .map(|(_, ps)| ps.agent_session.as_ref().map(|(_, sid)| sid.clone()))
            .collect();
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&store);

        let claimed: Vec<&String> = sessions.iter().flatten().collect();
        assert!(
            claimed.len() <= 1,
            "one session must not be claimed by two panes, got {sessions:?}"
        );
    }

    /// A hook-reported session belongs to its pane and must never be taken by
    /// another pane's cwd guess — even when the guesser is resolved first.
    /// Precise ids are claimed in a pass of their own before any guessing.
    #[test]
    fn a_hook_reported_session_outranks_another_panes_guess() {
        let _env = crate::persist::test_env("dup-session-hook");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.run_cmd(crate::app::keys::Cmd::SplitRight);
        let mut ids = app.layout().leaves();
        ids.sort_by_key(|p| p.0);
        let (older, newer) = (ids[0], ids[1]);

        let cwd = app.panes.get(&older).unwrap().cwd.clone();
        let store = std::env::temp_dir().join(format!("luvus-hooksess-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&store);
        // The real encoder, so a test can never drift from what luvus looks up.
        let proj = crate::agent::claude_project_dir(&store, &cwd);
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("owned.jsonl"), "{}").unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", &store);

        // The *newer* pane owns the session (the hook said so); the older pane
        // has none and would otherwise guess that very id first.
        for id in [older, newer] {
            app.status.get_mut(&id).unwrap().agent = "claude".into();
        }
        app.status.get_mut(&older).unwrap().agent_session = None;
        app.status.get_mut(&newer).unwrap().agent_session = Some(AgentSession {
            agent: "claude".into(),
            session_id: "owned".into(),
        });

        let snap = crate::persist::snapshot(&app);
        let by_pane: std::collections::HashMap<u32, Option<String>> = snap.workspaces[0].tabs[0]
            .panes
            .iter()
            .map(|(raw, ps)| (*raw, ps.agent_session.as_ref().map(|(_, s)| s.clone())))
            .collect();
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&store);

        assert_eq!(
            by_pane.get(&newer.0),
            Some(&Some("owned".to_string())),
            "the pane the hook named keeps its own session"
        );
        assert_eq!(
            by_pane.get(&older.0),
            Some(&None),
            "the other pane must not steal it, and restores as a plain shell"
        );
    }

    /// Forking then restarting must restore *two different* conversations.
    ///
    /// A fork shares its parent's folder and gets a brand new session id from the
    /// agent, so the fork's session immediately becomes the newest in that
    /// folder. With sessions resolved by "newest in this folder", the parent
    /// would be handed its own child's conversation and the fork would get
    /// nothing. Forking pins the parent to the session it was forked from, which
    /// leaves the fork free to claim the newer one.
    #[test]
    fn forking_then_restoring_keeps_parent_and_fork_on_their_own_sessions() {
        let _env = crate::persist::test_env("fork-session-split");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let parent = app.layout().focus;
        let cwd = app.panes.get(&parent).unwrap().cwd.clone();

        // A Claude store holding the parent's session.
        let store = std::env::temp_dir().join(format!("luvus-forksess-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&store);
        // The real encoder, so a test can never drift from what luvus looks up.
        let proj = crate::agent::claude_project_dir(&store, &cwd);
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("parent-sess.jsonl"), "{}").unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", &store);

        // The parent runs claude with no hook-reported id (the common case).
        {
            let st = app.status.get_mut(&parent).unwrap();
            st.agent = "claude".into();
            st.agent_session = None;
        }
        assert!(app.fork_pane(parent), "claude forks");
        let fork = app.layout().focus;
        assert_ne!(fork, parent);

        // Forking pinned the parent to the session it forked from.
        assert_eq!(
            app.status
                .get(&parent)
                .unwrap()
                .agent_session
                .as_ref()
                .map(|a| a.session_id.as_str()),
            Some("parent-sess"),
            "the parent keeps the conversation it was forked from"
        );

        // The agent now writes the fork's own (newer) session into the folder.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(proj.join("fork-sess.jsonl"), "{}").unwrap();
        app.status.get_mut(&fork).unwrap().agent = "claude".into();

        let snap = crate::persist::snapshot(&app);
        let by_pane: std::collections::HashMap<u32, Option<String>> = snap.workspaces[0].tabs[0]
            .panes
            .iter()
            .map(|(raw, ps)| (*raw, ps.agent_session.as_ref().map(|(_, s)| s.clone())))
            .collect();
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&store);

        assert_eq!(
            by_pane.get(&parent.0),
            Some(&Some("parent-sess".to_string())),
            "parent restores its own conversation, not the fork's"
        );
        assert_eq!(
            by_pane.get(&fork.0),
            Some(&Some("fork-sess".to_string())),
            "the fork restores the session it created"
        );
    }

    /// Reproduces: fork, restart, and the fork comes back as a bare shell.
    ///
    /// The parent is live, so its transcript keeps being appended to and is the
    /// *newest* file in the folder. The fork therefore guesses the parent's
    /// session, which the parent already claimed, and is left with nothing. The
    /// guess must fall through to the newest session **not already spoken for**.
    #[test]
    fn a_fork_resumes_even_when_the_parent_transcript_is_newer() {
        let _env = crate::persist::test_env("fork-newer-parent");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let parent = app.layout().focus;
        let cwd = app.panes.get(&parent).unwrap().cwd.clone();

        let store = std::env::temp_dir().join(format!("luvus-forknewer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&store);
        // The real encoder, so a test can never drift from what luvus looks up.
        let proj = crate::agent::claude_project_dir(&store, &cwd);
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("parent-sess.jsonl"), "{}").unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", &store);

        {
            let st = app.status.get_mut(&parent).unwrap();
            st.agent = "claude".into();
            st.agent_session = None;
        }
        assert!(app.fork_pane(parent), "claude forks");
        let fork = app.layout().focus;
        app.status.get_mut(&fork).unwrap().agent = "claude".into();

        // The fork writes its session, then the *parent* keeps working — so the
        // parent's transcript ends up with the newer mtime.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(proj.join("fork-sess.jsonl"), "{}").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(proj.join("parent-sess.jsonl"), "{}{}").unwrap();

        let snap = crate::persist::snapshot(&app);
        let by_pane: std::collections::HashMap<u32, Option<String>> = snap.workspaces[0].tabs[0]
            .panes
            .iter()
            .map(|(raw, ps)| (*raw, ps.agent_session.as_ref().map(|(_, s)| s.clone())))
            .collect();
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&store);

        assert_eq!(
            by_pane.get(&parent.0),
            Some(&Some("parent-sess".to_string()))
        );
        assert_eq!(
            by_pane.get(&fork.0),
            Some(&Some("fork-sess".to_string())),
            "the fork must still resume its own session, not fall back to a shell"
        );
    }

    /// Disk discovery cannot prove which of multiple sessions belongs to which
    /// unbound pane, even if their pane and session creation orders happen to
    /// match. It must preserve safety by leaving both panes unbound.
    #[test]
    fn ambiguous_unbound_sessions_do_not_guess_by_pane_age() {
        let _env = crate::persist::test_env("session-pairing");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let older = app.layout().focus;
        let cwd = app.panes.get(&older).unwrap().cwd.clone();

        let store = std::env::temp_dir().join(format!("luvus-pairing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&store);
        // The real encoder, so a test can never drift from what luvus looks up.
        let proj = crate::agent::claude_project_dir(&store, &cwd);
        std::fs::create_dir_all(&proj).unwrap();
        // The older pane's session was written first.
        std::fs::write(proj.join("old-sess.jsonl"), "{}").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(proj.join("new-sess.jsonl"), "{}").unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", &store);

        app.run_cmd(crate::app::keys::Cmd::SplitRight);
        let mut ids = app.layout().leaves();
        ids.sort_by_key(|p| p.0);
        let newer = *ids.last().unwrap();
        for id in [older, newer] {
            let st = app.status.get_mut(&id).unwrap();
            st.agent = "claude".into();
            st.agent_session = None;
        }

        let snap = crate::persist::snapshot(&app);
        let by_pane: std::collections::HashMap<u32, Option<String>> = snap.workspaces[0].tabs[0]
            .panes
            .iter()
            .map(|(raw, ps)| (*raw, ps.agent_session.as_ref().map(|(_, s)| s.clone())))
            .collect();
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&store);

        assert_eq!(by_pane.get(&newer.0), Some(&None));
        assert_eq!(by_pane.get(&older.0), Some(&None));
    }

    /// Regression: tabs do not establish a native agent-session boundary. A user
    /// can create the second tab first and then start its agent before starting
    /// one in the first tab, so global pane ids cannot be used to pair the two
    /// session files. Restarting must not move either conversation across tabs.
    #[test]
    fn ambiguous_sessions_across_tabs_are_not_reassigned() {
        let _env = crate::persist::test_env("cross-tab-session-pairing");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let first_tab_pane = app.layout().focus;
        let cwd = app.panes.get(&first_tab_pane).unwrap().cwd.clone();

        app.run_cmd(crate::app::keys::Cmd::NewTab);
        let second_tab_pane = app.layout().focus;
        assert_ne!(first_tab_pane, second_tab_pane);
        assert_eq!(app.ws().tabs.len(), 2, "two tabs are present");
        assert_eq!(app.panes.get(&second_tab_pane).unwrap().cwd, cwd);

        let store =
            std::env::temp_dir().join(format!("luvus-cross-tab-pairing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&store);
        // The real encoder, so a test can never drift from what luvus looks up.
        let proj = crate::agent::claude_project_dir(&store, &cwd);
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("first-session.jsonl"), "{}").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(proj.join("second-session.jsonl"), "{}").unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", &store);

        for id in [first_tab_pane, second_tab_pane] {
            let status = app.status.get_mut(&id).unwrap();
            status.agent = "claude".into();
            status.agent_session = None;
        }

        let snap = crate::persist::snapshot(&app);
        let by_pane: std::collections::HashMap<u32, Option<String>> = snap.workspaces[0]
            .tabs
            .iter()
            .flat_map(|tab| &tab.panes)
            .map(|(raw, ps)| (*raw, ps.agent_session.as_ref().map(|(_, sid)| sid.clone())))
            .collect();
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&store);

        assert_eq!(by_pane.get(&first_tab_pane.0), Some(&None));
        assert_eq!(by_pane.get(&second_tab_pane.0), Some(&None));
    }

    #[test]
    fn detect_tick_keeps_session_brand_when_screen_lacks_name() {
        // Regression: a pane with a resolved agent_session (from the integration
        // hook / disk discovery) must keep its brand — e.g. "claude" — even when
        // the on-screen banner doesn't contain the word "claude" that moment, so
        // classify() falls back to the bare shell name. Otherwise the reported
        // agent identity shown to UI/API consumers flaps to "zsh".
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let focus = app.layout().focus;

        let (reply, _r) = mpsc::channel();
        app.handle_api(&ApiRequest {
            id: "1".into(),
            method: "pane.report_session".into(),
            params: json!({"pane": focus.0.to_string(), "agent": "claude", "session_id": "abc-123"}),
            reply,
        });
        // A fresh shell pane's grid holds no "claude" banner, so the detect tick's
        // classify() falls back to the shell command — the exact trigger.
        app.detect_tick(Instant::now());
        assert_eq!(app.status.get(&focus).unwrap().agent, "claude");
    }

    #[test]
    fn detect_tick_dirties_when_an_idle_non_resumable_agent_appears() {
        let _env = crate::persist::test_env("idle-agent-sidebar-dirty");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let focus = app.layout().focus;
        assert!(
            !crate::agent::is_resumable("aider"),
            "the regression needs an agent whose identity is visible but not persisted"
        );

        app.proc_commands.insert(focus, vec!["aider".into()]);
        let now = Instant::now();
        app.last_detect_at = now - Duration::from_secs(1);
        app.last_proc_at = now;
        app.last_sessions_at = now;
        app.session_dirty = false;

        assert!(
            app.detect_tick(now),
            "an idle identity change adds a sidebar row and must request a frame"
        );
        assert_eq!(app.status.get(&focus).unwrap().agent, "aider");
        assert!(
            !app.session_dirty,
            "a non-resumable identity repaint does not create persistence work"
        );
    }

    #[test]
    fn unchanged_panes_reuse_cached_detection_text() {
        let _env = crate::persist::test_env("detect-generation-cache");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        let first = Instant::now();

        // Give the pane a deterministic engine. Its real shell reader keeps its
        // old engine, so stray output between ticks cannot race the extraction
        // counts; only the explicit advance below changes the generation.
        use std::sync::{Arc, Mutex};
        let (response_tx, _response_rx) = std::sync::mpsc::channel();
        app.panes.get_mut(&pane).unwrap().engine = Arc::new(Mutex::new(
            crate::terminal::vt::alacritty::AlacrittyEngine::new(
                80,
                24,
                response_tx,
                crate::config::SCROLLBACK_BYTES_DEFAULT,
            ),
        ));

        app.detect_tick(first);
        let extracted = app.detection_extractions;
        assert!(extracted > 0, "the first tick inspects every pane");

        app.detect_tick(first + Duration::from_millis(200));
        assert_eq!(
            app.detection_extractions, extracted,
            "an unchanged pane does not rebuild title or bottom text"
        );
        assert!(app.detection_skips > 0);

        if let Some(pane) = app.panes.get(&pane) {
            pane.engine.lock().unwrap().advance(b"new output\r\n");
        }
        app.detect_tick(first + Duration::from_millis(400));
        assert_eq!(app.detection_extractions, extracted + 1);
    }

    #[test]
    fn mouse_drag_resizes_pane_and_content_press_still_selects() {
        let _env = crate::persist::test_env("pane-resize-mouse");
        use crate::event::AppEvent;
        use ratatui::backend::TestBackend;
        use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::Terminal;

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        app.run_cmd(crate::app::keys::Cmd::SplitRight); // two side-by-side panes
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();

        let area = app.last_pane_area;
        let divs = app.layout().dividers(area);
        assert_eq!(divs.len(), 1, "one vertical divider");
        let line = divs[0].line;
        let leaves = app.layout().leaves();
        let left = leaves[0];
        let width = |app: &App, id| {
            app.layout()
                .panes(area)
                .into_iter()
                .find(|p| p.id == id)
                .unwrap()
                .rect
                .width
        };
        let before = width(&app, left);

        let mouse = |kind, col, row| MouseEvent {
            kind,
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        };
        // Grab the divider and drag it 20 cells left.
        assert!(app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            line,
            area.y + 2,
        ))));
        assert!(app.resize_drag.is_some(), "grabbed the divider");
        let target = line.saturating_sub(20);
        assert!(app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            target,
            area.y + 2,
        ))));
        assert!(app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            target,
            area.y + 2,
        ))));
        assert!(app.resize_drag.is_none(), "released the drag");
        assert!(
            width(&app, left) < before,
            "left pane narrowed: {before} -> {}",
            width(&app, left)
        );

        // A press deep inside a pane's content still starts a selection (no
        // regression): re-render so content rects reflect the new geometry.
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let right = *app.layout().leaves().last().unwrap();
        let content = app
            .pane_content_rects
            .iter()
            .find(|(id, _)| *id == right)
            .map(|(_, r)| *r)
            .expect("right pane content rect");
        assert!(app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            content.x + 3,
            content.y + 3,
        ))));
        assert!(app.resize_drag.is_none(), "content press is not a resize");
        assert!(app.selection.is_some(), "content press starts a selection");
        assert!(app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            content.x + 7,
            content.y + 3,
        ))));
    }

    #[test]
    fn clicking_another_pane_keeps_the_scrolled_pane_at_its_position() {
        let _env = crate::persist::test_env("scroll-focus-keeps-position");
        use crate::event::AppEvent;
        use ratatui::backend::TestBackend;
        use ratatui::crossterm::event::{
            KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
        };
        use ratatui::Terminal;
        use std::sync::{Arc, Mutex};

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        app.run_cmd(crate::app::keys::Cmd::SplitRight);
        let leaves = app.layout().leaves();
        let (left, right) = (leaves[0], leaves[1]);

        // Give the left pane deterministic history. Its real shell reader keeps
        // its old engine, so it cannot race this viewport assertion.
        let (response_tx, _response_rx) = std::sync::mpsc::channel();
        app.panes.get_mut(&left).unwrap().engine = Arc::new(Mutex::new(
            crate::terminal::vt::alacritty::AlacrittyEngine::new(
                60,
                38,
                response_tx,
                crate::config::SCROLLBACK_BYTES_DEFAULT,
            ),
        ));
        if let Some(pane) = app.panes.get(&left) {
            let mut engine = pane.engine.lock().unwrap();
            for i in 0..200 {
                engine.advance(format!("history {i}\r\n").as_bytes());
            }
        }

        app.layout_mut().focus = left;
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let rect = |app: &App, id| {
            app.pane_content_rects
                .iter()
                .find(|(pane, _)| *pane == id)
                .map(|(_, rect)| *rect)
                .unwrap()
        };
        let left_rect = rect(&app, left);
        let right_rect = rect(&app, right);
        let mouse = |kind, rect: Rect| {
            AppEvent::Mouse(MouseEvent {
                kind,
                column: rect.x + 2,
                row: rect.y + 2,
                modifiers: KeyModifiers::NONE,
            })
        };

        assert!(app.handle_event(mouse(MouseEventKind::ScrollUp, left_rect)));
        let stopped_at = app.panes.get(&left).unwrap().scroll_state().0;
        assert!(stopped_at > 0, "the left pane is stopped in its history");
        assert_eq!(app.scroll_pane, Some(left));

        assert!(app.handle_event(mouse(MouseEventKind::Down(MouseButton::Left), right_rect,)));
        assert_eq!(
            app.layout().focus,
            right,
            "the click focuses the right pane"
        );
        assert!(
            app.scroll_pane.is_none(),
            "the old pane no longer owns keyboard scroll mode"
        );
        assert_eq!(
            app.panes.get(&left).unwrap().scroll_state().0,
            stopped_at,
            "focus leaves the left viewport exactly where the user stopped"
        );

        assert!(!app.handle_event(AppEvent::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        ))));
        assert_eq!(app.layout().focus, right);
        assert_eq!(
            app.panes.get(&left).unwrap().scroll_state().0,
            stopped_at,
            "typing in the right pane cannot snap the left pane to live"
        );
    }

    #[test]
    fn dragging_the_left_sidebar_edge_resizes_and_persists() {
        let _env = crate::persist::test_env("sidebar-edge-drag");
        use crate::event::AppEvent;
        use ratatui::backend::TestBackend;
        use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::Terminal;

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();

        let seam = app
            .left_seam
            .expect("left sidebar is shown, so it has a seam");
        let before = app.sidebars.left.width;

        let mouse = |kind, col, row| MouseEvent {
            kind,
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        };
        // Grab the seam and drag it 6 columns to the right (wider).
        assert!(app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            seam.x,
            seam.y + 3,
        ))));
        assert_eq!(
            app.sidebar_resize,
            Some(Side::Left),
            "grabbed the left sidebar edge"
        );
        assert!(app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            seam.x + 6,
            seam.y + 3,
        ))));
        assert!(app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            seam.x + 6,
            seam.y + 3,
        ))));
        assert!(app.sidebar_resize.is_none(), "released the drag");
        assert_eq!(
            app.sidebars.left.width,
            before + 6,
            "the left sidebar widened by the drag distance"
        );
        // Released width is persisted for the next launch.
        assert_eq!(
            crate::config::load().sidebars.unwrap().left.width,
            before + 6,
            "the new width was written to config"
        );
    }

    #[test]
    fn sidebar_drag_clamps_and_never_crushes_the_content() {
        let _env = crate::persist::test_env("sidebar-edge-clamp");
        use crate::event::AppEvent;
        use ratatui::backend::TestBackend;
        use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::Terminal;

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let seam = app.left_seam.unwrap();

        let mouse = |kind, col, row| MouseEvent {
            kind,
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        };
        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            seam.x,
            seam.y + 3,
        )));
        // Drag far past the right edge: width caps at MAX, never eats the content.
        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            119,
            seam.y + 3,
        )));
        assert!(
            app.sidebars.left.width <= SIDEBAR_WIDTH_MAX,
            "width never exceeds the max ({} > {SIDEBAR_WIDTH_MAX})",
            app.sidebars.left.width
        );
        assert!(
            120 - app.sidebars.left.width >= 24,
            "the content keeps at least 24 columns"
        );
        // Drag hard left: width floors at MIN rather than collapsing to nothing.
        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            0,
            seam.y + 3,
        )));
        assert_eq!(
            app.sidebars.left.width, SIDEBAR_WIDTH_MIN,
            "a leftward drag floors at the minimum, not zero (closing is the chevron's job)"
        );
    }

    #[test]
    fn a_pane_click_does_not_start_a_sidebar_resize() {
        let _env = crate::persist::test_env("sidebar-edge-nopane");
        use crate::event::AppEvent;
        use ratatui::backend::TestBackend;
        use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::Terminal;

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let pane = app.last_pane_area;

        app.handle_event(AppEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: pane.x + 5,
            row: pane.y + 5,
            modifiers: KeyModifiers::NONE,
        }));
        assert!(
            app.sidebar_resize.is_none(),
            "a click inside a pane never grabs the sidebar edge"
        );
    }

    #[test]
    fn clicks_forward_to_a_mouse_tracking_app_instead_of_selecting() {
        // A pane app that requested mouse tracking (a TUI agent) receives
        // clicks — e.g. clicking a collapsed tool result expands it — instead
        // of luvus starting a text selection. Shift restores selection.
        let _env = crate::persist::test_env("mouse-forward");
        use crate::event::AppEvent;
        use ratatui::backend::TestBackend;
        use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::Terminal;

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        let id = app.layout().focus;
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let content = app
            .pane_content_rects
            .iter()
            .find(|(pid, _)| *pid == id)
            .map(|(_, r)| *r)
            .expect("pane content rect");

        // The app turns on button-event + SGR mouse tracking.
        app.panes
            .get(&id)
            .unwrap()
            .engine
            .lock()
            .unwrap()
            .advance(b"\x1b[?1002h\x1b[?1006h");

        let mouse = |kind, col, row, mods| MouseEvent {
            kind,
            column: col,
            row,
            modifiers: mods,
        };
        // Press inside the content: forwarded (grab held), no selection begun.
        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            content.x + 4,
            content.y + 2,
            KeyModifiers::NONE,
        )));
        let g = app.mouse_grab.expect("press grabbed for the app");
        assert_eq!(g.pane, id);
        assert_eq!(g.btn, 0);
        assert!(g.drag, "1002: drag tracking cached at press");
        assert!(g.sgr, "1006: SGR encoding cached at press");
        assert!(app.selection.is_none(), "no selection while forwarding");
        // Drag + release route to the app and close out the grab.
        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            content.x + 6,
            content.y + 2,
            KeyModifiers::NONE,
        )));
        assert!(app.selection.is_none());
        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            content.x + 6,
            content.y + 2,
            KeyModifiers::NONE,
        )));
        assert!(app.mouse_grab.is_none(), "release ends the grab");

        // Shift+click bypasses forwarding: luvus's own selection begins.
        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            content.x + 4,
            content.y + 2,
            KeyModifiers::SHIFT,
        )));
        assert!(app.mouse_grab.is_none());
        assert!(app.selection.is_some(), "shift+drag still selects text");

        // With tracking off, a plain click selects as before.
        app.selection = None;
        app.panes
            .get(&id)
            .unwrap()
            .engine
            .lock()
            .unwrap()
            .advance(b"\x1b[?1002l");
        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            content.x + 4,
            content.y + 2,
            KeyModifiers::NONE,
        )));
        assert!(app.mouse_grab.is_none());
        assert!(app.selection.is_some(), "no tracking → selection as before");
    }

    /// Tapping a split pane's ⤢ button zooms it to fullscreen, and tapping again
    /// restores the split — the touch-reachable equivalent of `Ctrl+Space z`
    /// (docs/18), so a phone with no keyboard can still expand a pane.
    #[test]
    fn tapping_the_zoom_button_toggles_fullscreen() {
        let _env = crate::persist::test_env("pane-zoom-tap");
        use ratatui::backend::TestBackend;
        use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::Terminal;

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        app.run_cmd(crate::app::keys::Cmd::SplitRight); // two side-by-side panes
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert_eq!(app.layout().len(), 2);
        assert!(!app.zoomed);

        // The focused pane shows a zoom button (the split panes are wide enough).
        let z = app.pane_zoom_rect.expect("focused pane has a ⤢ button");
        let tap = |app: &mut App, r: Rect| {
            app.handle_event(crate::event::AppEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: r.x + 1,
                row: r.y,
                modifiers: KeyModifiers::NONE,
            }));
        };
        tap(&mut app, z);
        assert!(app.zoomed, "tapping ⤢ zoomed the pane");

        // Zoomed: only one pane renders, and its button now restores the split.
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let z2 = app
            .pane_zoom_rect
            .expect("the zoomed pane keeps its button");
        tap(&mut app, z2);
        assert!(!app.zoomed, "tapping again restored the split");
    }

    /// The divider grab band must not reach into a pane's content.
    ///
    /// `RESIZE_GRAB_TOL` is ±2 cells so the seam is comfortable to hit, but the
    /// gap between panes is a single column — so the band used to overlap ~2
    /// columns of each neighbour's terminal content. Because `begin_resize` runs
    /// *before* selection and mouse-forwarding, a click a couple of cells from
    /// the seam started a divider drag instead of reaching the pane: it stole
    /// text selections and clicks meant for the agent underneath.
    #[test]
    fn resize_grab_zone_does_not_reach_into_pane_content() {
        let _env = crate::persist::test_env("resize-grab-content");
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        app.run_cmd(crate::app::keys::Cmd::SplitRight); // two side-by-side panes
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert_eq!(app.layout().len(), 2);

        // Every cell that belongs to a pane's content must be the pane's, even
        // the columns hugging the seam that the ±2 tolerance used to swallow.
        let rects: Vec<Rect> = app.pane_content_rects.iter().map(|(_, r)| *r).collect();
        assert_eq!(rects.len(), 2, "two content rects after the split");
        for rc in &rects {
            let mid = rc.y + rc.height / 2;
            for x in [rc.x, rc.right().saturating_sub(1)] {
                assert!(
                    !app.begin_resize(x, mid),
                    "clicking pane content at ({x},{mid}) must not grab a divider"
                );
                assert!(app.resize_drag.is_none());
                app.end_resize();
            }
        }

        // The seam itself is still grabbable: the gap/border column between the
        // two content rects belongs to no pane.
        let left = rects.iter().min_by_key(|r| r.x).unwrap();
        let right = rects.iter().max_by_key(|r| r.x).unwrap();
        let seam = (left.right() + right.x) / 2;
        let mid = left.y + left.height / 2;
        assert!(
            app.begin_resize(seam, mid),
            "the seam at ({seam},{mid}) still grabs the divider"
        );
        assert!(app.resize_drag.is_some(), "a drag started from the seam");
        app.end_resize();
    }

    #[test]
    fn resize_yields_to_pane_close_button() {
        let _env = crate::persist::test_env("resize-close-x");
        use crate::event::AppEvent;
        use ratatui::backend::TestBackend;
        use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::Terminal;

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        app.run_cmd(crate::app::keys::Cmd::SplitDown); // two stacked panes; focus = bottom
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert_eq!(app.layout().len(), 2);

        // The focused (bottom) pane's close ✕ sits on the top border — which is
        // the horizontal divider. Clicking it must close the pane, not resize.
        let x = app
            .pane_close_rect
            .expect("focused pane has a close button");
        let down = AppEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x.x + 1,
            row: x.y,
            modifiers: KeyModifiers::NONE,
        });
        app.handle_event(down);
        assert!(app.resize_drag.is_none(), "✕ click did not grab a divider");
        assert_eq!(app.layout().len(), 1, "✕ click closed the pane");
    }

    /// On a stacked split the horizontal divider lands on the lower pane's top
    /// border — the very row that holds its title, ⤢ zoom, and ✕ close. A resize
    /// grab must yield to all of that chrome (not just ✕), or those controls are
    /// unclickable; the non-chrome part of the seam must still grab the divider.
    #[test]
    fn resize_yields_to_pane_title_and_zoom_but_still_grabs_the_seam() {
        let _env = crate::persist::test_env("resize-title-zoom");
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        app.run_cmd(crate::app::keys::Cmd::SplitDown); // two stacked panes; focus = bottom
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert_eq!(app.layout().len(), 2);

        // The focused (bottom) pane's chrome all sits on its top border = the
        // divider row. Every piece must yield to the click, not grab a resize.
        let zoom = app
            .pane_zoom_rect
            .expect("focused pane has a ⤢ zoom button");
        let (_, title) = *app
            .pane_title_rects
            .iter()
            .max_by_key(|(_, rc)| rc.y) // the lower pane's title
            .expect("the lower pane has a title strip");
        for (name, rc) in [("zoom", zoom), ("title", title)] {
            assert!(
                !app.begin_resize(rc.x + 1, rc.y),
                "clicking the {name} must not grab the divider"
            );
            assert!(app.resize_drag.is_none(), "{name}: no resize started");
            app.end_resize();
        }

        // The bare middle of the same border row (between the title and the
        // buttons) is not chrome, so it still grabs the divider to resize.
        let divider_row = zoom.y;
        let bare = 60u16; // mid-width: past the title, before the right-edge buttons
        assert!(
            !app.on_pane_chrome(bare, divider_row),
            "the chosen seam cell is genuinely not chrome"
        );
        assert!(
            app.begin_resize(bare, divider_row),
            "the non-chrome seam at ({bare},{divider_row}) still grabs the divider"
        );
        assert!(app.resize_drag.is_some(), "a drag started from the seam");
        app.end_resize();
    }

    /// End-to-end through the real mouse pipeline (`handle_event`), the user's
    /// exact report: on a stacked split, clicking the bottom pane's title opens
    /// its command overlay, clicking its ⤢ zoom toggles zoom, and clicking its
    /// content focuses it — none of them get eaten by a divider resize.
    #[test]
    fn stacked_bottom_pane_title_zoom_and_body_are_all_clickable() {
        let _env = crate::persist::test_env("stacked-clickable");
        use crate::event::AppEvent;
        use ratatui::backend::TestBackend;
        use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::Terminal;

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        let top = app.layout().focus;
        app.run_cmd(crate::app::keys::Cmd::SplitDown); // bottom pane, now focused
        let bottom = app.layout().focus;
        assert_ne!(top, bottom);
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        let render = |app: &mut App, term: &mut Terminal<TestBackend>| {
            term.draw(|f| crate::ui::render(f, app)).unwrap();
        };
        let click = |app: &mut App, x: u16, y: u16| {
            app.handle_event(AppEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: x,
                row: y,
                modifiers: KeyModifiers::NONE,
            }));
        };
        render(&mut app, &mut term);

        // 1. The ⤢ zoom button on the bottom pane's top border toggles zoom.
        let zoom = app.pane_zoom_rect.expect("bottom pane has a ⤢ button");
        click(&mut app, zoom.x + 1, zoom.y);
        assert!(app.zoomed, "clicking ⤢ zoomed the split");
        assert!(app.resize_drag.is_none(), "⤢ did not start a resize");
        app.zoomed = false;
        render(&mut app, &mut term);

        // 2. The title strip opens the running-command overlay.
        let (_, title) = *app
            .pane_title_rects
            .iter()
            .max_by_key(|(_, rc)| rc.y)
            .expect("bottom pane has a title strip");
        click(&mut app, title.x + 1, title.y);
        assert!(
            app.cmd_inspect.is_some(),
            "clicking the title opened the command overlay"
        );
        assert!(app.resize_drag.is_none(), "title did not start a resize");
        app.close_cmd_inspect();
        render(&mut app, &mut term);

        // 3. Focus the top pane, then a click in the bottom pane's *body* focuses
        //    it back — the pane is clickable, not blocked by the divider band.
        app.layout_mut().focus = top;
        let body = app
            .pane_content_rects
            .iter()
            .find(|(id, _)| *id == bottom)
            .map(|(_, r)| *r)
            .expect("bottom pane has a content rect");
        // A cell near the top of the body, inside the ±2 divider band — the case
        // most likely to be wrongly swallowed.
        click(&mut app, body.x + 2, body.y);
        assert_eq!(
            app.layout().focus,
            bottom,
            "a click in the bottom pane body focused it"
        );
        assert!(
            app.resize_drag.is_none(),
            "body click did not start a resize"
        );
    }

    /// A node that can't be opened must say so. `create_workspace_at` used to
    /// swallow a failed shell spawn: it returned normally, `active_ws` still
    /// pointed at the previously focused node, and the user was left looking at
    /// the *wrong folder* with no error anywhere.
    ///
    /// An unresolvable shell is the failure that actually reaches this branch,
    /// and it is the plausible Windows one: `resolve_shell` tries `pwsh.exe`,
    /// then `powershell.exe`, then `%COMSPEC%`, and a bad `config.shell` or
    /// `LUVUS_SHELL` defeats all three. (A *missing directory*, by contrast,
    /// still spawns — the child only fails once it execs — so it is deliberately
    /// not what this asserts on.)
    #[test]
    fn failing_to_open_a_node_is_reported_not_swallowed() {
        let _env = crate::persist::test_env("ws-open-failure");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).expect("spawn pane");
        let before = app.workspaces.len();
        let active_before = app.active_ws;

        app.config.shell = "luvus-not-a-real-shell-4b1c9f".to_string();
        assert!(
            !app.create_workspace_at(std::env::temp_dir()),
            "opening a node whose shell cannot start must report failure"
        );
        assert_eq!(app.workspaces.len(), before, "no half-built node is added");
        assert_eq!(app.active_ws, active_before, "focus must not move");
        // The UHP must not answer with the *previously* active node, which
        // read as success to `luvus` itself and to any scripting agent.
        let (reply, _r) = std::sync::mpsc::channel();
        let resp = app.handle_api(&crate::ipc::api::ApiRequest {
            id: "1".into(),
            method: "workspace.open".into(),
            params: serde_json::json!({ "path": std::env::temp_dir().display().to_string() }),
            reply,
        });
        let res: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(
            res.pointer("/error/code").and_then(|v| v.as_str()),
            Some("spawn_failed"),
            "a failed open is an API error, not a fake success: {res}"
        );

        assert!(
            app.toast
                .as_ref()
                .is_some_and(|(t, _)| t.contains("couldn't open")),
            "the user is told, rather than silently shown the previous folder"
        );
    }

    /// docs/43 WIN-6: `workspace.open` matched nodes with raw `PathBuf` equality,
    /// so a different *spelling* of an open folder added a duplicate node instead
    /// of focusing it.
    ///
    /// The spelling used here is the `\\?\` verbatim prefix, because it is the
    /// one that discriminates on *every* platform: `Path`'s own `==` already
    /// normalizes trailing separators, and case-folding is Windows-only (both
    /// are covered directly in `platform`'s tests). Without `same_path` this
    /// test sees two nodes.
    #[test]
    fn opening_an_already_open_node_focuses_it_instead_of_duplicating() {
        let _env = crate::persist::test_env("ws-open-dedupe");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).expect("spawn pane");

        let dir = std::env::temp_dir().join("luvus-dedupe-4b1c9f");
        std::fs::create_dir_all(&dir).expect("temp dir");
        assert!(app.create_workspace_at(dir.clone()), "first open succeeds");
        let count = app.workspaces.len();
        let opened = app.active_ws;

        // The same folder as `canonicalize` would hand it back on Windows.
        let spelled = format!(r"\\?\{}", dir.display());
        let (reply, _r) = std::sync::mpsc::channel();
        let resp = app.handle_api(&crate::ipc::api::ApiRequest {
            id: "1".into(),
            method: "workspace.open".into(),
            params: serde_json::json!({ "path": spelled }),
            reply,
        });
        let res: serde_json::Value = serde_json::from_str(&resp).unwrap();

        assert!(
            res.get("error").is_none(),
            "re-opening an open node is not an error: {res}"
        );
        assert_eq!(
            app.workspaces.len(),
            count,
            "the existing node is focused, not duplicated"
        );
        assert_eq!(app.active_ws, opened, "focus lands on the existing node");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The reported bug: a relocated WORKSPACES dock's rows opened files instead
    /// of switching workspace. Root cause — the FILES dock's hit geometry
    /// (`file_tree_rects`/`files_area`) was the one dock geometry *not* zeroed at
    /// the top of a frame, so when the FILES dock isn't drawn its stale row rects
    /// linger and, since a left click tests `file_tree_rects` before `ws_rects`,
    /// swallow clicks meant for a WORKSPACES row sitting in the same cells.
    #[test]
    fn stale_files_rects_do_not_swallow_workspace_clicks() {
        let _env = crate::persist::test_env("ws-dock-stale-files");
        use crate::event::AppEvent;
        use ratatui::backend::TestBackend;
        use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::layout::Rect;
        use ratatui::Terminal;

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        // A second workspace so there is another row to switch to.
        app.create_workspace_at(std::env::temp_dir());
        assert!(
            app.workspaces.len() >= 2,
            "two workspaces to switch between"
        );

        // Simulate stale FILES geometry left from a frame where the dock *was*
        // drawn (the FILES dock is not in the default sidebars, so it won't draw
        // now). Cover the whole screen so it would otherwise eat every click.
        app.file_tree_rects = vec![(0, Rect::new(0, 0, 120, 40))];
        app.files_area = Rect::new(0, 0, 120, 40);

        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();

        // The fix zeroes the FILES geometry because the dock wasn't drawn.
        assert!(
            app.file_tree_rects.is_empty(),
            "stale FILES row rects were cleared"
        );
        assert_eq!(app.files_area, Rect::ZERO, "stale FILES area was cleared");

        // So clicking a WORKSPACES row now switches to it instead of opening a file.
        assert!(
            !app.ws_rects.is_empty(),
            "the WORKSPACES dock drew its rows"
        );
        let (target, rect) = app
            .ws_rects
            .iter()
            .find(|(i, _)| *i != app.active_ws)
            .map(|(i, r)| (*i, *r))
            .expect("a different workspace row to click");
        app.handle_event(AppEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x + 1,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(
            app.active_ws, target,
            "the workspace-row click switched workspace (did not open a file)"
        );
    }

    /// Stability guard for the whole dock system (docs/29): whatever a dock — a
    /// built-in *or* a user module dock — leaves in the app's click geometry, one
    /// frame where that dock isn't drawn (its sidebar hidden) must zero it, so no
    /// stale rect can fire under a widened pane area or a relocated dock. This is
    /// the invariant the WORKSPACES-vs-FILES bug violated; it now covers every
    /// dock geometry field at once, so a future dock can't reintroduce it.
    #[test]
    fn hidden_sidebar_zeroes_all_dock_geometry_including_module_docks() {
        let _env = crate::persist::test_env("dock-geometry-reset");
        use ratatui::backend::TestBackend;
        use ratatui::layout::Rect;
        use ratatui::Terminal;

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();

        // Seed every dock's hit geometry with stale rects, as if each had drawn.
        let junk = Rect::new(0, 0, 40, 40);
        app.workspaces_area = junk;
        app.agents_area = junk;
        app.files_area = junk;
        app.file_tree_rects = vec![(0, junk)];
        app.agents_filter_rects = vec![(true, junk)];
        app.workspace_branch_rects = vec![(0, junk)];
        app.module_dock_rects = vec![("example.buzz".into(), 0, junk)]; // a user module dock

        // Hide both sidebars so no dock draws this frame — the worst stale case.
        app.sidebars.left.visible = false;
        app.sidebars.right.visible = false;

        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();

        assert_eq!(app.workspaces_area, Rect::ZERO, "WORKSPACES area cleared");
        assert_eq!(app.agents_area, Rect::ZERO, "AGENTS area cleared");
        assert_eq!(app.files_area, Rect::ZERO, "FILES area cleared");
        assert!(app.file_tree_rects.is_empty(), "FILES row rects cleared");
        assert!(app.agents_filter_rects.is_empty(), "AGENTS filter cleared");
        assert!(
            app.workspace_branch_rects.is_empty(),
            "branch rects cleared"
        );
        assert!(
            app.module_dock_rects.is_empty(),
            "user module dock rects cleared — a stale module dock can't fire"
        );
    }

    #[test]
    fn pane_menu_splits_closes_and_skips_dashboards() {
        let _env = crate::persist::test_env("pane-menu");
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();

        // Right-click opens the menu; a render fills its clickable item rects.
        let pane = app.layout().focus;
        app.open_pane_menu(pane, 6, 6);
        assert!(app.pane_menu.is_some(), "menu opened");
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();

        // Clicking "Split vertical" adds a pane and closes the menu.
        let (item, rect) = app.pane_menu.as_ref().unwrap().items[0];
        assert_eq!(item, PaneMenuItem::SplitVertical);
        let before = app.layout().len();
        app.pane_menu_click(rect.x + 1, rect.y);
        assert!(app.pane_menu.is_none(), "menu closed after a click");
        assert_eq!(
            app.layout().len(),
            before + 1,
            "split vertical added a pane"
        );

        // Split horizontal and close, via the action path.
        app.open_pane_menu(app.layout().focus, 6, 6);
        app.pane_menu_action(PaneMenuItem::SplitHorizontal);
        assert_eq!(
            app.layout().len(),
            before + 2,
            "split horizontal added a pane"
        );
        app.open_pane_menu(app.layout().focus, 6, 6);
        app.pane_menu_action(PaneMenuItem::Close);
        assert_eq!(app.layout().len(), before + 1, "close removed a pane");

        // A dashboard tab has no panes to act on — the menu never opens there.
        app.run_cmd(crate::app::keys::Cmd::OpenBoard);
        app.open_pane_menu(app.layout().focus, 6, 6);
        assert!(app.pane_menu.is_none(), "no pane menu on the orch board");
    }

    /// A module dock row shows the menu **it declared** (docs/52), and that menu
    /// is a snapshot: a `ui.dock.push` while it is open — which pollers do on a
    /// timer — cannot make a click run a different row's action.
    #[test]
    fn dock_row_menu_is_declared_per_row_and_survives_a_repaint() {
        let _env = crate::persist::test_env("dock-menu");
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();

        let row = |text: &str, value: &str, menu: Vec<DockRowMenuItem>| DockRow {
            text: text.into(),
            dot: None,
            action: Some("select".into()),
            value: Some(value.into()),
            menu,
        };
        let item = |title: &str, action: &str, destructive: bool| DockRowMenuItem {
            title: title.into(),
            action: action.into(),
            value: None,
            destructive,
        };
        let board_menu = || {
            vec![
                item("Flash this board", "flash", false),
                item("", "", false), // divider
                item("Erase flash", "erase", true),
            ]
        };

        app.push_module_dock(
            "devices",
            Some("DEVICES".into()),
            Side::Left,
            vec![
                row("esp32s3", "/dev/ttyA", board_menu()),
                row("build", "build", Vec::new()), // a command row: no menu
            ],
        );

        // Rows in one dock differ: a device offers actions, a command offers
        // none. luvus cannot invent items for a row it does not understand, so
        // an undeclared menu opens nothing rather than an empty popup.
        app.open_dock_menu("devices", 1, 6, 6);
        assert!(app.dock_menu.is_none(), "row without a menu opens nothing");

        app.open_dock_menu("devices", 0, 6, 6);
        {
            let menu = app.dock_menu.as_ref().expect("declared menu opens");
            assert_eq!(menu.items.len(), 3);
            assert_eq!(menu.row_value.as_deref(), Some("/dev/ttyA"));
        }

        // A render fills the clickable rects.
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert_eq!(app.dock_menu.as_ref().unwrap().rects.len(), 3);

        // ── the property that matters (docs/52 §5) ──────────────────────────
        // A poller replaces the whole row list underneath the open menu.
        app.push_module_dock(
            "devices",
            None,
            Side::Left,
            vec![row("esp32c6", "/dev/ttyZ", board_menu())],
        );
        {
            let menu = app.dock_menu.as_ref().expect("a repaint doesn't close it");
            assert_eq!(menu.row_text, "esp32s3", "still the row that was clicked");
            assert_eq!(menu.row_value.as_deref(), Some("/dev/ttyA"));
            assert_eq!(menu.items[2].title, "Erase flash");
        }

        // A divider is inert: clicking it keeps the menu open.
        let d = app.dock_menu.as_ref().unwrap().rects[1];
        app.dock_menu_click(d.x + 1, d.y);
        assert!(
            app.dock_menu.is_some(),
            "a divider click keeps the menu open"
        );

        // Clicking a real item runs *that* item and closes the menu. No module
        // is installed in this test, so the invocation surfaces as a toast
        // naming the action it tried — which is the snapshotted `erase`, not
        // whatever the repainted row at index 0 would have given.
        let r = app.dock_menu.as_ref().unwrap().rects[2];
        app.dock_menu_click(r.x + 1, r.y);
        assert!(app.dock_menu.is_none(), "menu closed after an item click");
        let toast = app
            .toast
            .as_ref()
            .map(|(t, _)| t.clone())
            .unwrap_or_default();
        assert!(
            toast.contains("erase"),
            "ran the snapshotted action; toast was {toast:?}"
        );

        // Clicking outside dismisses without running anything.
        app.open_dock_menu("devices", 0, 6, 6);
        assert!(app.dock_menu.is_some());
        app.dock_menu_click(119, 39);
        assert!(app.dock_menu.is_none(), "a click outside dismisses");
    }

    /// A real right-click on a rendered module dock row reaches the dock menu
    /// (docs/52) — and a row with no declared menu does **not** fall through to
    /// the pane menu sitting underneath the sidebar.
    #[test]
    fn right_click_on_a_module_dock_row_opens_its_menu() {
        let _env = crate::persist::test_env("dock-menu-rclick");
        use ratatui::backend::TestBackend;
        use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::Terminal;

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        app.push_module_dock(
            "devices",
            Some("DEVICES".into()),
            Side::Left,
            vec![
                DockRow {
                    text: "esp32s3".into(),
                    dot: Some("done".into()),
                    action: Some("select".into()),
                    value: Some("/dev/ttyA".into()),
                    menu: vec![DockRowMenuItem {
                        title: "Flash this board".into(),
                        action: "flash".into(),
                        value: None,
                        destructive: false,
                    }],
                },
                DockRow {
                    text: "build".into(),
                    dot: None,
                    action: Some("build".into()),
                    value: None,
                    menu: Vec::new(),
                },
            ],
        );

        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();

        let rect_for = |app: &App, i: usize| {
            app.module_dock_rects
                .iter()
                .find(|(d, r, _)| d == "devices" && *r == i)
                .map(|(_, _, rect)| *rect)
                .expect("dock row drew")
        };
        let rclick = |app: &mut App, rect: Rect| {
            app.handle_event(AppEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Right),
                column: rect.x + 1,
                row: rect.y,
                modifiers: KeyModifiers::NONE,
            }));
        };

        let r0 = rect_for(&app, 0);
        rclick(&mut app, r0);
        let menu = app.dock_menu.as_ref().expect("right-click opened the menu");
        assert_eq!(menu.items[0].title, "Flash this board");
        assert_eq!(menu.row_value.as_deref(), Some("/dev/ttyA"));
        app.dock_menu = None;

        // The command row declares no menu: nothing opens, and crucially the
        // click does not leak through to the pane menu.
        let r1 = rect_for(&app, 1);
        rclick(&mut app, r1);
        assert!(app.dock_menu.is_none(), "no menu for an undeclared row");
        assert!(app.pane_menu.is_none(), "and no pane menu underneath it");
    }

    /// Moving a pane to another tab re-parents its id between layout trees — the
    /// process/PTY survives (never through `close_pane`) — and if the source tab
    /// empties it collapses, with focus following the pane.
    #[test]
    fn move_pane_to_tab_reparents_and_keeps_the_process() {
        let _env = crate::persist::test_env("pane-move");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();

        // Tab 0 holds pane `p`. A new tab 1 holds its own pane.
        let p = app.layout().focus;
        app.run_cmd(crate::app::keys::Cmd::NewTab);
        assert_eq!(app.ws().active_tab, 1, "new tab is active");
        let tab1_pane = app.layout().focus;
        assert_ne!(p, tab1_pane);
        app.workspaces[0].active_tab = 0; // back to the pane we'll move

        // The submenu offers tab #1 as a target.
        let targets = app.pane_move_targets();
        assert!(
            targets.iter().any(|(t, _)| *t == MoveTarget::Tab(1)),
            "tab 1 is a move target"
        );

        // Move `p` into tab 1. Tab 0 (now empty) is removed, so the target becomes
        // the only tab, holding both panes; the process is untouched.
        app.move_pane_to_tab(p, MoveTarget::Tab(1))
            .expect("valid destination tab");
        assert!(
            app.panes.contains_key(&p),
            "the pane's process survived the move"
        );
        assert_eq!(app.ws().tabs.len(), 1, "the emptied source tab collapsed");
        let leaves = app.ws().tabs[0].layout.leaves();
        assert!(
            leaves.contains(&p) && leaves.contains(&tab1_pane),
            "both panes now live in the target tab"
        );
        assert_eq!(app.ws().active_tab, 0, "focus followed the pane's new tab");
        assert_eq!(app.layout().focus, p, "the moved pane is focused");
    }

    /// End-to-end: right-click a pane, hover "Move to tab" so the submenu opens
    /// and fills its tab rects, then click a tab row to move the pane there.
    #[test]
    fn move_to_tab_submenu_opens_on_hover_and_moves_on_click() {
        let _env = crate::persist::test_env("pane-move-menu");
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        let p = app.layout().focus;
        app.run_cmd(crate::app::keys::Cmd::NewTab); // tab 1
        app.workspaces[0].active_tab = 0;

        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        app.open_pane_menu(p, 6, 6);
        app.hover = None;
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();

        // Hover the "Move to tab" row → next render opens the submenu.
        let mrect = app
            .pane_menu
            .as_ref()
            .unwrap()
            .items
            .iter()
            .find(|(it, _)| *it == PaneMenuItem::MoveToTab)
            .map(|(_, r)| *r)
            .expect("Move to tab row is present");
        app.hover = Some((mrect.x + 1, mrect.y));
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert!(app.pane_menu.as_ref().unwrap().move_open, "submenu opened");
        let tab_rects = app.pane_menu.as_ref().unwrap().tab_rects.clone();
        assert!(!tab_rects.is_empty(), "submenu listed tabs");

        // Click the tab #1 row → the pane moves and the menu closes.
        let (_, r) = tab_rects
            .iter()
            .find(|(tg, _)| *tg == MoveTarget::Tab(1))
            .expect("tab 1 offered");
        app.pane_menu_click(r.x + 1, r.y);
        assert!(app.pane_menu.is_none(), "menu closed after moving");
        assert!(app.panes.contains_key(&p), "pane survived");
        assert!(
            app.ws().tabs.iter().any(|t| t.layout.leaves().contains(&p)),
            "pane landed in a tab"
        );
    }

    #[test]
    fn pane_move_validates_before_detaching_and_excludes_dashboards() {
        let _env = crate::persist::test_env("pane-move-validation");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        let pane = app.layout().focus;
        let original = app.layout().leaves();

        assert_eq!(
            app.move_pane_to_tab(pane, MoveTarget::Tab(0)),
            Err(PaneMoveError::SameTab)
        );
        assert_eq!(
            app.move_pane_to_tab(pane, MoveTarget::Tab(99)),
            Err(PaneMoveError::TargetOutOfRange)
        );
        assert_eq!(
            app.move_pane_to_tab(pane, MoveTarget::NewTab),
            Err(PaneMoveError::NoChange)
        );
        assert_eq!(app.layout().leaves(), original, "failed moves are atomic");

        app.open_mission_control(0);
        app.workspaces[0].active_tab = 0;
        assert!(
            !app.pane_move_targets()
                .iter()
                .any(|(target, _)| *target == MoveTarget::Tab(1)),
            "dashboard tabs are not offered as pane destinations"
        );
        assert_eq!(
            app.move_pane_to_tab(pane, MoveTarget::Tab(1)),
            Err(PaneMoveError::TargetNotPaneTab)
        );
        assert_eq!(
            app.layout().leaves(),
            original,
            "pane remains in its source tab"
        );
    }

    #[test]
    fn tab_move_preserves_the_active_tab_identity() {
        let _env = crate::persist::test_env("tab-move");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        app.workspaces[0].tabs[0].name = Some("a".into());
        app.run_cmd(crate::app::keys::Cmd::NewTab);
        app.workspaces[0].tabs[1].name = Some("b".into());
        app.run_cmd(crate::app::keys::Cmd::NewTab);
        app.workspaces[0].tabs[2].name = Some("c".into());
        assert_eq!(app.ws().active_tab, 2);

        // Move A from first to last. C stays active and shifts from slot 3 to 2.
        assert_eq!(app.move_tab(0, 2), Ok(1));
        let names = app
            .ws()
            .tabs
            .iter()
            .map(|tab| tab.name.clone().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names, ["b", "c", "a"]);
        assert_eq!(
            app.ws().tabs[app.ws().active_tab].name.as_deref(),
            Some("c")
        );

        // Move the active C to the first slot; focus follows that same tab.
        assert_eq!(app.move_tab(1, 0), Ok(0));
        let names = app
            .ws()
            .tabs
            .iter()
            .map(|tab| tab.name.clone().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names, ["c", "b", "a"]);
        assert_eq!(app.ws().active_tab, 0);
        assert!(app.session_dirty);

        let before = names;
        assert_eq!(app.move_tab(0, 0), Err(TabMoveError::SamePosition));
        assert_eq!(app.move_tab(0, 9), Err(TabMoveError::PositionOutOfRange));
        let after = app
            .ws()
            .tabs
            .iter()
            .map(|tab| tab.name.clone().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(after, before, "failed tab moves leave the order untouched");
    }

    #[test]
    fn tab_move_is_correct_for_every_source_destination_and_active_position() {
        let _env = crate::persist::test_env("tab-move-exhaustive");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        let original = ["a", "b", "c", "d"];

        for active in 0..original.len() {
            for from in 0..original.len() {
                for to in 0..original.len() {
                    if from == to {
                        continue;
                    }
                    app.workspaces[0].tabs = original
                        .iter()
                        .map(|name| {
                            let mut tab = Tab::panes(TileLayout::new(PaneId::alloc()));
                            tab.name = Some((*name).to_string());
                            tab
                        })
                        .collect();
                    app.workspaces[0].active_tab = active;

                    let active_name = original[active];
                    let mut expected = original.to_vec();
                    let moved = expected.remove(from);
                    expected.insert(to, moved);

                    let new_active = app.move_tab(from, to).expect("valid permutation");
                    let actual = app
                        .ws()
                        .tabs
                        .iter()
                        .map(|tab| tab.name.as_deref().unwrap())
                        .collect::<Vec<_>>();
                    assert_eq!(actual, expected, "active={active}, from={from}, to={to}");
                    assert_eq!(
                        app.ws().tabs[new_active].name.as_deref(),
                        Some(active_name),
                        "active tab identity changed: active={active}, from={from}, to={to}"
                    );
                }
            }
        }
    }

    #[test]
    fn tab_context_menu_reorders_swaps_and_renames_its_exact_target() {
        let _env = crate::persist::test_env("tab-context-menu");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        app.workspaces[0].tabs = ["a", "b", "c"]
            .into_iter()
            .map(|name| {
                let mut tab = Tab::panes(TileLayout::new(PaneId::alloc()));
                tab.name = Some(name.to_string());
                tab
            })
            .collect();
        app.workspaces[0].active_tab = 2;

        app.open_tab_menu(1, 10, 2);
        assert_eq!(
            app.tab_menu_items(),
            [
                TabMenuItem::Rename,
                TabMenuItem::MoveLeft,
                TabMenuItem::MoveRight,
                TabMenuItem::SwapWith,
            ]
        );
        app.tab_menu_action(TabMenuItem::MoveLeft);
        let names = app
            .ws()
            .tabs
            .iter()
            .map(|tab| tab.name.as_deref().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names, ["b", "a", "c"]);
        assert_eq!(app.ws().active_tab, 2, "active C stays active");

        // The open menu follows B by identity even if an API reorder occurs
        // before its action is clicked.
        app.open_tab_menu(0, 10, 2);
        app.move_tab(0, 2).unwrap();
        app.tab_menu_action(TabMenuItem::MoveLeft);
        let names = app
            .ws()
            .tabs
            .iter()
            .map(|tab| tab.name.as_deref().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names, ["a", "b", "c"]);

        // Swap With resolves both snapshots after an intervening reorder,
        // then swaps those exact tabs rather than their old numeric positions.
        app.open_tab_menu(0, 10, 2);
        let target = app
            .tab_menu
            .as_ref()
            .unwrap()
            .swap_targets
            .iter()
            .find(|(_, label)| label == "c")
            .unwrap()
            .0
            .clone();
        app.tab_menu.as_mut().unwrap().swap_rects = vec![(target, Rect::new(20, 4, 8, 1))];
        app.move_tab(0, 1).unwrap();
        app.tab_menu_click(21, 4);
        let names = app
            .ws()
            .tabs
            .iter()
            .map(|tab| tab.name.as_deref().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names, ["b", "c", "a"]);
        assert_eq!(app.ws().active_tab, 1, "active C follows its new position");

        app.open_tab_menu(1, 10, 2);
        app.tab_menu_action(TabMenuItem::Rename);
        let rename_target = app.tab_rename.as_ref().unwrap().target.clone();
        assert_eq!(app.resolve_tab_menu_target(&rename_target), Some((0, 1)));

        app.tab_menu = None;
        app.open_tab_menu(3, 10, 2); // the tab bar's `+` pseudo-index
        assert!(app.tab_menu.is_none());
    }

    #[test]
    fn keyboard_resize_mode_enters_resizes_and_exits() {
        let _env = crate::persist::test_env("pane-resize-keys");
        use crate::event::AppEvent;
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        app.last_pane_area = Rect::new(0, 0, 120, 40);
        app.run_cmd(crate::app::keys::Cmd::SplitRight);
        let key = |code, m| AppEvent::Key(KeyEvent::new(code, m));

        // Ctrl+Space then `r` enters resize mode.
        app.handle_event(key(KeyCode::Char(' '), KeyModifiers::CONTROL));
        app.handle_event(key(KeyCode::Char('r'), KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Resize);

        let area = app.last_pane_area;
        let focus = app.layout().focus; // the new (right) pane
        let width = |app: &App| {
            app.layout()
                .panes(area)
                .into_iter()
                .find(|p| p.id == focus)
                .unwrap()
                .rect
                .width
        };
        let before = width(&app);
        // Left arrow grows the focused right pane (moves the divider left).
        app.handle_event(key(KeyCode::Left, KeyModifiers::NONE));
        assert!(width(&app) > before, "arrow resized the focused pane");

        // Esc leaves resize mode.
        app.handle_event(key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn tab_rename_sets_name_persists_and_excludes_dashboards() {
        let _env = crate::persist::test_env("tab-rename");
        use crate::event::AppEvent;
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let ch = |c| AppEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        let code = |c| AppEvent::Key(KeyEvent::new(c, KeyModifiers::NONE));

        // Rename tab 0 → "build".
        app.open_tab_rename(0);
        assert!(app.tab_rename.is_some(), "rename modal opened");
        for c in "build".chars() {
            app.handle_event(ch(c));
        }
        app.handle_event(code(KeyCode::Enter));
        assert!(app.tab_rename.is_none(), "modal closed on Enter");
        assert_eq!(app.ws().tabs[0].name.as_deref(), Some("build"));

        // Persists across snapshot → restore.
        let json = serde_json::to_string(&persist::snapshot(&app)).unwrap();
        let snap: SessionSnapshot = serde_json::from_str(&json).unwrap();
        let (tx2, _rx2) = std::sync::mpsc::channel();
        let restored = App::from_snapshot(snap, tx2).unwrap();
        assert_eq!(restored.ws().tabs[0].name.as_deref(), Some("build"));

        // Clearing the name (empty on Enter) reverts to the number.
        app.open_tab_rename(0);
        for _ in 0.."build".len() {
            app.handle_event(code(KeyCode::Backspace));
        }
        app.handle_event(code(KeyCode::Enter));
        assert_eq!(app.ws().tabs[0].name, None, "empty name clears the label");

        // The orchestration board (a dashboard tab) cannot be renamed.
        app.run_cmd(crate::app::keys::Cmd::OpenBoard);
        let board_idx = app.ws().active_tab;
        assert!(app.ws().tabs[board_idx].is_orch());
        app.open_tab_rename(board_idx);
        assert!(app.tab_rename.is_none(), "dashboard tab is not renameable");
    }

    #[test]
    fn orchestration_flow_over_the_api() {
        // End-to-end wiring of ORCH-1/2 through the JSON control API (docs/22 M0):
        // add → dep-gated claim → path leases (overlap denied) → done releases the
        // lease + unlocks the dependent. `test_env` gives a fresh empty LUVUS_HOME so
        // orch.json writes to a temp dir and App::new loads a clean ledger.
        let _env = crate::persist::test_env("orch");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let a = app.layout().focus;
        // A second real pane for the lease-conflict case.
        app.handle_event(key(' ', KeyModifiers::CONTROL));
        app.handle_event(key('v', KeyModifiers::NONE));
        let b = *app.layout().leaves().iter().find(|id| **id != a).unwrap();

        fn call(app: &mut App, method: &str, params: Value) -> Value {
            let (reply, _r) = mpsc::channel();
            let resp = app.handle_api(&ApiRequest {
                id: "1".into(),
                method: method.into(),
                params,
                reply,
            });
            serde_json::from_str(&resp).unwrap()
        }

        // Two tasks; t2 depends on t1.
        let r = call(
            &mut app,
            "task.add",
            json!({"title":"auth","paths":["src/auth/**"]}),
        );
        assert_eq!(r["result"]["task"]["id"], "t1");
        call(&mut app, "task.add", json!({"title":"api","deps":["t1"]}));

        // t2 can't be claimed while its dependency is unfinished.
        let r = call(
            &mut app,
            "task.claim",
            json!({"id":"t2","pane": a.0.to_string()}),
        );
        assert_eq!(r["error"]["code"], "deps_unmet");

        // Claim t1, lease its paths for pane A.
        let r = call(
            &mut app,
            "task.claim",
            json!({"id":"t1","pane": a.0.to_string()}),
        );
        assert_eq!(r["result"]["task"]["status"], "claimed");
        let r = call(
            &mut app,
            "lease.acquire",
            json!({"task":"t1","paths":["src/auth/**"],"pane": a.0.to_string()}),
        );
        assert_eq!(r["result"]["lease"]["id"], "l1");

        // Pane B asking for an overlapping path is denied with the holder.
        let r = call(
            &mut app,
            "lease.acquire",
            json!({"task":"t2","paths":["src/auth/token.rs"],"pane": b.0.to_string()}),
        );
        assert_eq!(r["error"]["code"], "lease_conflict");

        // Finishing t1 releases its lease and unlocks t2.
        let r = call(&mut app, "task.done", json!({"id":"t1"}));
        assert_eq!(r["result"]["task"]["status"], "done");
        let r = call(
            &mut app,
            "task.claim",
            json!({"id":"t2","pane": b.0.to_string()}),
        );
        assert_eq!(r["result"]["task"]["status"], "claimed");
        // The formerly-conflicting path is now free for pane B.
        let r = call(
            &mut app,
            "lease.acquire",
            json!({"task":"t2","paths":["src/auth/token.rs"],"pane": b.0.to_string()}),
        );
        assert!(
            r.get("result").is_some(),
            "lease should be granted after release: {r}"
        );
    }

    #[test]
    fn workspace_open_focuses_existing_or_creates_new() {
        // `luvus` attaching from a new folder → `workspace.open` adds it; from a
        // folder that's already a workspace → it just focuses it (no duplicate).
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let initial = app.ws().cwd.clone();
        let n = app.workspaces.len();

        let open = |app: &mut App, path: &std::path::Path| {
            let (reply, _r) = mpsc::channel();
            app.handle_api(&ApiRequest {
                id: "1".into(),
                method: "workspace.open".into(),
                params: json!({ "path": path.display().to_string() }),
                reply,
            });
        };

        // Re-opening the initial folder just focuses it — no new workspace.
        open(&mut app, &initial);
        assert_eq!(app.workspaces.len(), n, "existing folder isn't duplicated");

        // Opening a different folder adds + focuses it.
        let other = std::env::temp_dir();
        open(&mut app, &other);
        assert_eq!(
            app.workspaces.len(),
            n + 1,
            "new folder becomes a workspace"
        );
        assert_eq!(app.ws().cwd, other, "the new workspace is focused");
    }

    #[test]
    fn attach_open_does_not_steal_focus_from_the_active_workspace() {
        // The automatic attach-open (`focus: false`) must add the launch folder if
        // new, but never yank you off the workspace a restored session left you on.
        // This is the "reopen snaps back to the first workspace" bug.
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let first = app.ws().cwd.clone();
        // A second workspace, now the active one (like being on a non-first node).
        let other = std::env::temp_dir();
        let open = |app: &mut App, path: &std::path::Path, focus: bool| {
            let (reply, _r) = mpsc::channel();
            app.handle_api(&ApiRequest {
                id: "1".into(),
                method: "workspace.open".into(),
                params: json!({ "path": path.display().to_string(), "focus": focus }),
                reply,
            });
        };
        open(&mut app, &other, true);
        let active = app.active_ws;
        assert!(active > 0, "on a non-first workspace");

        // Attach-open of the launch (first) folder must NOT switch away from it.
        open(&mut app, &first, false);
        assert_eq!(
            app.active_ws, active,
            "focus:false kept the active workspace on reopen"
        );

        // An explicit open (focus:true) still focuses the folder.
        open(&mut app, &first, true);
        assert_eq!(app.ws().cwd, first, "explicit open still focuses");
    }

    #[test]
    fn resume_session_opens_pane() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let before_panes = app.panes.len();
        let before_ws = app.workspaces.len();

        app.resumable = vec![crate::agent::SessionInfo {
            agent: "claude".into(),
            session_id: "abc".into(),
            cwd: std::env::temp_dir().join("luvus-resume-test"),
            updated: std::time::SystemTime::now(),
        }];
        app.resume_session(0);

        assert_eq!(app.panes.len(), before_panes + 1, "a pane was spawned");
        assert_eq!(
            app.workspaces.len(),
            before_ws + 1,
            "a new workspace for the cwd"
        );
        let s = app.status.get(&app.layout().focus).unwrap();
        assert_eq!(s.agent, "claude");
        assert_eq!(s.agent_session.as_ref().unwrap().session_id, "abc");
        assert!(app.resumable.is_empty(), "session dropped from the list");
    }

    #[test]
    fn fork_pane_splits_a_fork_capable_agent() {
        let _env = crate::persist::test_env("fork-pane");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let src = app.layout().focus;
        let before = app.layout().len();

        // A plain pane can't fork: no agent, no session.
        assert!(!app.fork_pane(src), "non-agent pane does not fork");
        assert_eq!(app.layout().len(), before);

        // Mark it as a live Claude pane with a known session id (as the hook
        // would), then fork: a new pane splits in beside it, source untouched.
        {
            let st = app.status.get_mut(&src).unwrap();
            st.agent = "claude".into();
            st.agent_session = Some(AgentSession {
                agent: "claude".into(),
                session_id: "sess-abc".into(),
            });
        }
        assert!(app.fork_pane(src), "fork-capable agent forks");
        assert_eq!(app.layout().len(), before + 1, "a fork pane was spawned");
        // Both the source and the new fork pane are present in the same tab.
        let leaves = app.layout().leaves();
        assert!(leaves.contains(&src), "source pane survives the fork");
        assert_eq!(leaves.len(), before + 1);
        // The focused (new) pane is tagged as the same agent.
        let new = app.layout().focus;
        assert_ne!(new, src);
        assert_eq!(app.status.get(&new).unwrap().agent, "claude");
    }

    #[test]
    fn fork_pane_splits_a_grok_session() {
        let _env = crate::persist::test_env("fork-pane-grok");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let src = app.layout().focus;
        {
            let st = app.status.get_mut(&src).unwrap();
            st.agent = "grok".into();
            st.agent_session = Some(AgentSession {
                agent: "grok".into(),
                session_id: "sess-grok".into(),
            });
        }
        assert!(app.fork_pane(src), "grok sessions fork natively");
        let new = app.layout().focus;
        assert_ne!(new, src);
        assert_eq!(app.status.get(&new).unwrap().agent, "grok");
        assert_eq!(
            app.status
                .get(&src)
                .unwrap()
                .agent_session
                .as_ref()
                .unwrap()
                .session_id,
            "sess-grok"
        );
    }

    #[test]
    fn fork_from_mission_control_uses_the_agents_real_tab() {
        let _env = crate::persist::test_env("fork-pane-mission");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let source = app.layout().focus;
        {
            let status = app.status.get_mut(&source).unwrap();
            status.agent = "claude".into();
            status.agent_session = Some(AgentSession {
                agent: "claude".into(),
                session_id: "sess-mission-fork".into(),
            });
        }

        app.open_mission_control(0);
        assert!(app.active_is_mission());
        assert!(app.fork_pane(source), "Mission Control can fork its row");
        assert!(
            !app.active_is_mission(),
            "focus follows the fork's real tab"
        );
        let leaves = app.layout().leaves();
        assert_eq!(leaves.len(), 2);
        assert!(leaves.contains(&source));
        assert_ne!(app.layout().focus, source, "the fork is focused");
    }

    #[test]
    fn codex_pane_menu_offers_and_runs_native_fork() {
        let _env = crate::persist::test_env("fork-codex-pane-menu");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let src = app.layout().focus;
        let before = app.layout().len();
        {
            let st = app.status.get_mut(&src).unwrap();
            st.agent = "codex".into();
            st.agent_session = Some(AgentSession {
                agent: "codex".into(),
                session_id: "019c1234-abcd-7890-abcd-ef0123456789".into(),
            });
        }

        app.open_pane_menu(src, 1, 1);
        assert!(
            app.pane_menu_items().contains(&PaneMenuItem::ForkPane),
            "right-click menu exposes fork for Codex"
        );
        app.pane_menu_action(PaneMenuItem::ForkPane);

        assert_eq!(app.layout().len(), before + 1, "Codex fork opens a pane");
        let fork = app.layout().focus;
        assert_ne!(fork, src);
        assert_eq!(app.status.get(&fork).unwrap().agent, "codex");
    }

    #[test]
    fn codex_pane_menu_hides_fork_without_an_exact_session() {
        let _env = crate::persist::test_env("fork-codex-menu-unbound");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let pane = app.layout().focus;
        app.status.get_mut(&pane).unwrap().agent = "codex".into();

        app.open_pane_menu(pane, 1, 1);

        assert!(
            !app.pane_menu_items().contains(&PaneMenuItem::ForkPane),
            "Codex needs the exact session bound to this pane before Fork is offered"
        );
    }

    #[test]
    fn sidebar_lists_scroll() {
        use ratatui::backend::TestBackend;
        use ratatui::crossterm::event::{MouseEvent, MouseEventKind};
        use ratatui::Terminal;

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        for _ in 0..9 {
            app.new_workspace(); // 10 workspaces — more than fit in a short sidebar
        }
        app.active_ws = 0;
        app.last_active_ws_shown = 0;

        let mut term = Terminal::new(TestBackend::new(80, 18)).unwrap();
        let mut draw = |app: &mut App| {
            term.draw(|f| crate::ui::render(f, app))
                .map(|_| ())
                .unwrap()
        };
        draw(&mut app);
        assert!(
            app.workspaces_area.height > 0,
            "the workspaces list was measured"
        );
        assert_eq!(app.workspaces_scroll, 0);

        let na = app.workspaces_area;
        let wheel = |app: &mut App, kind| {
            app.handle_event(AppEvent::Mouse(MouseEvent {
                kind,
                column: na.x + 2,
                row: na.y + 1,
                modifiers: KeyModifiers::NONE,
            }))
        };
        // Wheel down over the WORKSPACES list → it scrolls.
        assert!(wheel(&mut app, MouseEventKind::ScrollDown));
        assert!(wheel(&mut app, MouseEventKind::ScrollDown));
        draw(&mut app);
        assert_eq!(
            app.workspaces_scroll, 2,
            "wheel scrolled the workspaces list down"
        );
        // Wheel up past the top → clamps at 0.
        for _ in 0..5 {
            wheel(&mut app, MouseEventKind::ScrollUp);
        }
        draw(&mut app);
        assert_eq!(app.workspaces_scroll, 0, "scroll clamps at the top");
        // Selecting an off-screen workspace auto-reveals it.
        app.active_ws = 9;
        draw(&mut app);
        assert!(
            app.workspaces_scroll > 0,
            "the active workspace was scrolled into view"
        );
    }

    #[test]
    fn agent_menu_resumes_and_dismisses_a_session() {
        use ratatui::backend::TestBackend;
        use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        use ratatui::Terminal;

        let sess = |id: &str, p: &str| crate::agent::SessionInfo {
            agent: "claude".into(),
            session_id: id.into(),
            cwd: PathBuf::from(p),
            updated: std::time::SystemTime::now(),
        };
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.resumable = vec![sess("s0", "/p/a"), sess("s1", "/p/b")];
        app.agents_active_only = false; // show the resumable history

        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();

        // Right-click the second session row → an AGENTS menu with Resume + Close.
        let row = app.session_rects.iter().find(|(i, _)| *i == 1).unwrap().1;
        app.handle_event(AppEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: row.x + 1,
            row: row.y,
            modifiers: KeyModifiers::NONE,
        }));
        assert!(
            app.agent_menu.is_some(),
            "right-click opened the agent menu"
        );
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let items = &app.agent_menu.as_ref().unwrap().items;
        assert_eq!(items.len(), 2, "session menu has Resume + Close");
        assert_eq!(items[0].0, AgentMenuItem::Resume);

        // Click "Close" → the session leaves the list and stays dismissed.
        let close = items[1].1;
        app.handle_event(AppEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: close.x + 1,
            row: close.y,
            modifiers: KeyModifiers::NONE,
        }));
        assert!(app.agent_menu.is_none(), "menu closed after a click");
        assert!(
            app.resumable.iter().all(|s| s.session_id != "s1"),
            "session removed from the list"
        );
        assert!(
            app.dismissed_sessions.contains("s1"),
            "stays dismissed across rescans"
        );
    }

    #[test]
    fn settings_modal_interactions() {
        use ratatui::backend::TestBackend;
        use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        use ratatui::Terminal;

        // Isolate config I/O to a temp dir so this is deterministic.
        let _env = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("luvus-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("LUVUS_HOME", &tmp);

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();

        assert!(app.settings.is_none());
        app.open_settings();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert_eq!(app.settings_tab_rects.len(), 7, "seven tabs");
        assert!(
            !app.settings_ctl_rects.is_empty(),
            "the opening tab lists controls"
        );
        // Settings opens on General (the first tab); step to Theme for the
        // live-preview assertions below.
        assert_eq!(
            app.settings.as_ref().unwrap().tab,
            crate::app::SettingsTab::General
        );
        app.handle_event(AppEvent::Key(KeyEvent::new(
            KeyCode::Tab,
            KeyModifiers::NONE,
        )));
        assert_eq!(
            app.settings.as_ref().unwrap().tab,
            crate::app::SettingsTab::Theme
        );
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let text: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("Settings") && text.contains("Theme") && text.contains("Agents"));

        // Moving the selection down live-applies the next theme.
        assert_eq!(app.config.theme, "quattro-rally");
        app.handle_event(AppEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )));
        assert_eq!(app.config.theme, crate::ui::theme::THEMES[1]); // next after noir

        let click = |app: &mut App, x, y| {
            app.handle_event(AppEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: x,
                row: y,
                modifiers: KeyModifiers::NONE,
            }));
        };
        // Click the Layout tab, then toggle "Pane titles". Its index is derived
        // from `layout_rows()` rather than hardcoded, so inserting a row above it
        // (e.g. Scrollback) can't silently point this test at the wrong control.
        let layout = app
            .settings_tab_rects
            .iter()
            .find(|(t, _)| *t == SettingsTab::Layout)
            .unwrap()
            .1;
        click(&mut app, layout.x + 1, layout.y);
        assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::Layout);
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let before = app.config.layout.show_titles;
        let titles_idx = app
            .layout_rows()
            .iter()
            .position(|r| matches!(r, LayoutRow::PaneTitles))
            .expect("the Layout tab has a Pane titles row");
        let row = app
            .settings_ctl_rects
            .iter()
            .find(|(i, _)| *i == titles_idx)
            .unwrap()
            .1;
        click(&mut app, row.x + 2, row.y);
        assert_ne!(
            app.config.layout.show_titles, before,
            "click toggles pane titles"
        );

        // Esc closes.
        app.handle_event(AppEvent::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )));
        assert!(app.settings.is_none());

        std::env::remove_var("LUVUS_HOME");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn ui_renders_in_the_selected_language() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let text = |term: &Terminal<TestBackend>| -> String {
            term.backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect()
        };

        // English baseline shows the English sidebar header.
        app.catalog = crate::i18n::by_code("en");
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert!(text(&term).contains("WORKSPACES"), "EN header");

        // A Latin language swaps the header text (ESPACIOS = WORKSPACES, contiguous).
        app.catalog = crate::i18n::by_code("es");
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let es = text(&term);
        assert!(es.contains("ESPACIOS"), "translated header appears");
        assert!(!es.contains("WORKSPACES"), "English header replaced");

        // CJK renders too (`工` = first char of the zh header). A wide char's
        // trailing cell is a space, so we check the lead glyph, not the pair.
        app.catalog = crate::i18n::by_code("zh");
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert!(text(&term).contains('工'), "CJK header renders");
    }

    #[test]
    fn modals_render_in_the_selected_language() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.catalog = crate::i18n::by_code("es");
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let text = |term: &Terminal<TestBackend>| -> String {
            term.backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect()
        };

        // The menu button (sidebar) is translated.
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert!(text(&term).contains("Menú"), "menu button translated");

        // The folder picker ("open new workspace" modal) is translated.
        app.open_folder_picker();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert!(
            text(&term).contains("Abrir esta carpeta"),
            "picker rows translated"
        );
        assert!(
            text(&term).contains("Abrir espacio"),
            "picker title translated"
        );
        app.close_folder_picker();

        // The `?` cheat-sheet body (command labels) is translated.
        app.help_open = true;
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert!(
            text(&term).contains("Cerrar panel"),
            "cheat-sheet command labels translated"
        );
    }

    #[test]
    fn settings_modal_widens_to_fit_wide_language_tabs() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        // Japanese tab labels (CJK, width-2) are wider than the old 74-col cap.
        app.catalog = crate::i18n::by_code("ja");
        app.open_settings();
        // A terminal with room: the modal must grow so all 7 tabs render (the
        // Language tab was previously clipped off the right edge).
        let mut term = Terminal::new(TestBackend::new(120, 30)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let modal = app.settings_modal_rect.expect("settings modal is rendered");
        assert_eq!(modal.height, 28, "the taller modal uses the available room");
        assert!(modal.width <= 120 && modal.right() <= 120);
        assert_eq!(
            app.settings_tab_rects.len(),
            7,
            "all 7 tabs render (none clipped)"
        );
        assert!(
            app.settings_tab_rects
                .iter()
                .any(|(t, _)| *t == SettingsTab::Language),
            "the Language tab is present"
        );

        // Small terminals still clamp the enlarged modal to the viewport.
        let mut narrow = Terminal::new(TestBackend::new(32, 12)).unwrap();
        narrow.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let modal = app.settings_modal_rect.expect("narrow settings modal");
        assert!(modal.width <= 32 && modal.height <= 12);
        assert!(modal.right() <= 32 && modal.bottom() <= 12);
    }

    #[test]
    fn settings_language_tab_swaps_catalog_and_persists() {
        use ratatui::backend::TestBackend;
        use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        use ratatui::Terminal;

        let _env = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("luvus-lang-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("LUVUS_HOME", &tmp);

        let (tx, _rx) = std::sync::mpsc::channel();
        // Wide enough that all 8 tabs render (Language is the last one).
        let mut app = App::new(120, 24, tx).unwrap();
        let mut term = Terminal::new(TestBackend::new(120, 24)).unwrap();
        app.open_settings();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert_eq!(app.config.language, "en");

        // Click the Language tab.
        let lang = app
            .settings_tab_rects
            .iter()
            .find(|(t, _)| *t == SettingsTab::Language)
            .unwrap()
            .1;
        app.handle_event(AppEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: lang.x + 1,
            row: lang.y,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::Language);
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();

        // Moving the selection picks the next language — applied live + persisted.
        app.handle_event(AppEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )));
        assert_ne!(
            app.config.language, "en",
            "a non-default language is selected"
        );
        assert_eq!(
            app.catalog.workspaces,
            crate::i18n::by_code(&app.config.language).workspaces,
            "catalog swapped live"
        );
        assert_eq!(
            crate::config::load().language,
            app.config.language,
            "persisted to config.json"
        );

        std::env::remove_var("LUVUS_HOME");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn arrow_keys_focus_panes_and_rebinding_works() {
        let _env = crate::persist::test_env("arrow-keys");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();

        // Split right (Ctrl+Space v) → focus moves to the new right pane.
        app.handle_event(key(' ', KeyModifiers::CONTROL));
        app.handle_event(key('v', KeyModifiers::NONE));
        let right = app.layout().focus;
        // Prefix + ← arrow focuses the left pane (the headline new binding).
        app.handle_event(key(' ', KeyModifiers::CONTROL));
        app.handle_event(AppEvent::Key(KeyEvent::new(
            KeyCode::Left,
            KeyModifiers::NONE,
        )));
        assert_ne!(
            app.layout().focus,
            right,
            "← moved focus off the right pane"
        );

        // Rebind "New tab" from `c` to `t` through Settings → Keys.
        app.open_settings();
        app.handle_event(key('4', KeyModifiers::NONE)); // Keys tab (Theme/Layout/Notify/Keys)
        assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::Keys);
        let idx = Cmd::ALL.iter().position(|c| *c == Cmd::NewTab).unwrap();
        if let Some(ui) = app.settings.as_mut() {
            ui.cursor = idx + crate::app::KEYS_HEADER_ROWS;
        }
        app.handle_event(AppEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ))); // capture
        assert!(app.settings.as_ref().unwrap().capturing);
        app.handle_event(key('t', KeyModifiers::NONE)); // bind to `t`
        assert!(!app.settings.as_ref().unwrap().capturing);
        assert_eq!(app.key_for(Cmd::NewTab), "t");
        app.close_settings();

        // `t` now makes a tab; the old `c` no longer does.
        let tabs = app.ws().tabs.len();
        app.handle_event(key(' ', KeyModifiers::CONTROL));
        app.handle_event(key('t', KeyModifiers::NONE));
        assert_eq!(app.ws().tabs.len(), tabs + 1, "rebound key works");
        app.handle_event(key(' ', KeyModifiers::CONTROL));
        app.handle_event(key('c', KeyModifiers::NONE));
        assert_eq!(app.ws().tabs.len(), tabs + 1, "old default freed");
    }

    #[test]
    fn settings_slider_arrows_step_both_ways() {
        use ratatui::backend::TestBackend;
        use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        use ratatui::Terminal;

        let _env = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("luvus-slider-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("LUVUS_HOME", &tmp);

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        app.open_settings(); // General
        for _ in 0..2 {
            app.handle_event(AppEvent::Key(KeyEvent::new(
                KeyCode::Tab,
                KeyModifiers::NONE,
            ))); // General → Theme → Layout
        }
        assert_eq!(
            app.settings.as_ref().unwrap().tab,
            crate::app::SettingsTab::Layout
        );
        let sidebar_width_row = app
            .layout_rows()
            .iter()
            .position(|row| matches!(row, crate::app::settings::LayoutRow::SidebarWidth))
            .expect("the Layout tab has a Left sidebar width row");
        app.settings.as_mut().unwrap().cursor = sidebar_width_row;
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let left = app
            .settings_arrow_rects
            .iter()
            .find(|(row, d, _)| *row == sidebar_width_row && *d < 0)
            .unwrap()
            .2;
        let right = app
            .settings_arrow_rects
            .iter()
            .find(|(row, d, _)| *row == sidebar_width_row && *d > 0)
            .unwrap()
            .2;
        let click = |app: &mut App, r: Rect| {
            app.handle_event(AppEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: r.x,
                row: r.y,
                modifiers: KeyModifiers::NONE,
            }));
        };
        let start = app.sidebars.left.width;
        click(&mut app, left);
        assert!(
            app.sidebars.left.width < start,
            "left arrow decreases width"
        );
        let low = app.sidebars.left.width;
        click(&mut app, right);
        assert!(app.sidebars.left.width > low, "right arrow increases width");

        std::env::remove_var("LUVUS_HOME");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // A working agent shows an animated rotating-circle spinner in the AGENTS
    // list dot slot (not the static `●`), advancing with `App.spinner`.
    // Clicking a pane's title opens the running-command overlay. The point is
    // that the command comes from the OS, not the screen: an agent's own UI
    // elides long commands and those characters never reach luvus at all.
    #[test]
    fn clicking_a_pane_title_shows_the_real_command() {
        use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        use ratatui::{backend::TestBackend, Terminal};
        let _env = crate::persist::test_env("cmd-inspect");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();

        // Titles (and borders) only render on split panes, so split first — the
        // single-pane case is covered by the pane context menu instead.
        app.split(Axis::Col);
        let id = app.layout().focus;
        // Render once so the title strips are registered as click targets.
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let (_, title) = *app
            .pane_title_rects
            .iter()
            .find(|(pid, _)| *pid == id)
            .expect("the focused pane has a clickable title");

        assert!(app.cmd_inspect.is_none());
        app.handle_event(AppEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: title.x + 1,
            row: title.y,
            modifiers: KeyModifiers::NONE,
        }));
        let c = app.cmd_inspect.as_ref().expect("the overlay opened");
        assert_eq!(c.pane, id);
        // The pane's own shell is the root of the tree, with its real argv.
        assert!(
            c.procs.first().is_some_and(|p| p.depth == 0),
            "the pane's shell is the root: {:?}",
            c.procs
        );
        // It renders, and any key dismisses it.
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        app.handle_event(key('q', KeyModifiers::NONE));
        assert!(app.cmd_inspect.is_none(), "any key closes the overlay");
    }

    #[test]
    fn working_agent_shows_spinner() {
        use ratatui::{backend::TestBackend, Terminal};
        // Isolate `$LUVUS_HOME`: with the developer's real config a different
        // dock layout can push the AGENTS rows out of view, so the spinner is
        // never drawn and this fails depending on test order.
        let _env = crate::persist::test_env("spinner");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        // Make the default pane a working "claude" agent so it lists as active.
        let pid = *app.panes.keys().next().unwrap();
        let mut ps = PaneStatus::new("claude".into());
        ps.state = crate::ui::theme::State::Working;
        app.status.insert(pid, ps);

        // Take the frame set from the theme rather than hardcoding glyphs, so
        // changing the spinner's look never silently breaks this test.
        let frames: Vec<&str> = (0..crate::ui::theme::SPINNER_FRAMES)
            .map(crate::ui::theme::spinner_frame)
            .collect();
        let frame_at = |app: &mut App, spin: u64| -> String {
            app.spinner = spin;
            let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
            term.draw(|f| crate::ui::render(f, app)).unwrap();
            let buf = term.backend().buffer().clone();
            // The dot is the first glyph of the agent row inside the sidebar.
            (0..buf.area.height)
                .flat_map(|r| (0..buf.area.width).map(move |c| (c, r)))
                .filter_map(|(c, r)| buf.cell((c, r)).map(|x| x.symbol().to_string()))
                .find(|s| frames.contains(&s.as_str()))
                .unwrap_or_default()
        };
        let f0 = frame_at(&mut app, 0);
        let f1 = frame_at(&mut app, 1);
        assert!(!f0.is_empty(), "a working agent shows a spinner glyph");
        assert_ne!(f0, f1, "the spinner advances with app.spinner");
    }

    // An agent that finishes a working stretch (Working → Idle) queues the retro
    // chime, whether or not its pane is focused.
    #[test]
    fn agent_finish_plays_sound() {
        let _env = crate::persist::test_env("chime");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        // The chime is optional: both sounds ship disabled.
        assert!(
            !app.config.notifications.sound_on_done && !app.config.notifications.sound_on_blocked,
            "sounds are off by default"
        );
        app.config.notifications.sound_on_done = true;

        let pid = *app.panes.keys().next().unwrap();
        let now = std::time::Instant::now() + std::time::Duration::from_millis(200);
        let mut ps = PaneStatus::new("claude".into());
        ps.state = crate::ui::theme::State::Working; // currently working
        ps.candidate = crate::ui::theme::State::Idle; // wants idle…
        ps.candidate_since = now - std::time::Duration::from_secs(5); // …and has held long enough
        ps.last_activity = now - std::time::Duration::from_secs(5); // quiet → classifies Idle
        ps.agent_session = Some(AgentSession {
            agent: "claude".into(),
            session_id: "s".into(),
        });
        app.status.insert(pid, ps);

        assert!(!app.pending_sound);
        app.detect_tick(now);
        assert!(
            app.pending_sound,
            "an agent finishing its working stretch plays the chime"
        );
    }

    // docs/07 regression: scrolling a pane back into history must never report
    // the agent as working. Scrollback preserves the spinner / "esc to interrupt"
    // frames of earlier turns, so reading the *scrolled* viewport made an idle
    // agent flip to Working the moment the user scrolled up to read something.
    #[test]
    fn scrolling_back_does_not_read_as_working() {
        use crate::ui::theme::State;
        let _env = crate::persist::test_env("scroll-state");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let id = app.layout().focus;

        // An earlier turn printed a spinner + interrupt hint; it has long since
        // scrolled off, and the live screen is a quiet prompt.
        if let Some(p) = app.panes.get(&id) {
            if let Ok(mut e) = p.engine.lock() {
                e.advance("⠹ Thinking… (esc to interrupt)\r\n".as_bytes());
                for i in 0..60 {
                    e.advance(format!("output line {i}\r\n").as_bytes());
                }
                e.advance(b"$ \r\n");
            }
        }
        {
            let s = app.status.get_mut(&id).unwrap();
            s.agent = "claude".into();
            s.state = State::Idle;
            s.last_activity = std::time::Instant::now() - Duration::from_secs(5);
        }
        let t0 = std::time::Instant::now();
        app.detect_tick(t0);
        assert_eq!(
            app.status.get(&id).unwrap().state,
            State::Idle,
            "a quiet agent starts idle"
        );

        // Scroll up until that old marker is genuinely back on screen.
        if let Some(p) = app.panes.get(&id) {
            p.scroll(60);
        }
        app.detection_dirty.insert(id);
        let visible = app
            .panes
            .get(&id)
            .and_then(|p| p.engine.lock().ok().map(|e| e.visible_rows().join("\n")))
            .unwrap_or_default();
        assert!(
            visible.contains("esc to interrupt"),
            "precondition: the stale marker is visible in the scrolled viewport"
        );

        // It is on screen, but it is history — the agent is still idle.
        app.detect_tick(t0 + Duration::from_millis(200));
        assert_eq!(
            app.status.get(&id).unwrap().state,
            State::Idle,
            "scrolling into history must not report the agent as working"
        );
    }

    // docs/07 regression: switching tabs / clicking into an agent pane makes the
    // agent repaint, and that repaint rarely contains the agent's own name. The
    // pane's identity has to stay sticky, or it gets re-classified as a plain
    // shell and the repaint's output reads as "working".
    #[test]
    fn repaint_without_the_agent_name_stays_idle() {
        use crate::ui::theme::State;
        let _env = crate::persist::test_env("repaint-identity");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let id = app.layout().focus;

        // The pane is a known agent (as a hook or disk discovery would bind it)…
        {
            let s = app.status.get_mut(&id).unwrap();
            s.agent = "claude".into();
            s.state = State::Idle;
        }
        // …and it repaints a prompt box that never says "claude" and has no
        // working marker — exactly what a tab switch produces.
        if let Some(p) = app.panes.get(&id) {
            if let Ok(mut e) = p.engine.lock() {
                let mut buf = vec![b'\n'; 20];
                buf.extend_from_slice(b"> \r\n  ? for shortcuts\r\n");
                e.advance(&buf);
            }
        }
        let now = std::time::Instant::now();
        app.status.get_mut(&id).unwrap().last_activity = now; // the repaint is output

        app.detect_tick(now);
        assert_eq!(
            app.status.get(&id).unwrap().state,
            State::Idle,
            "a repaint must not read as the agent working"
        );
        assert_eq!(
            app.status.get(&id).unwrap().agent,
            "claude",
            "and the pane keeps its identity"
        );
    }

    // docs/07: the same recent output reads Idle while the user is typing (echo)
    // but Working when the agent is generating (no recent input).
    #[test]
    fn typing_is_not_mistaken_for_agent_working() {
        use crate::ui::theme::State;
        let _env = crate::persist::test_env("typing");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let id = app.layout().focus;
        let now = std::time::Instant::now() + std::time::Duration::from_millis(200);

        // There is recent output on the pane either way (fresh last_activity).
        app.status.get_mut(&id).unwrap().state = State::Idle;
        app.status.get_mut(&id).unwrap().last_activity = now;

        // The user just typed: the recent output is keystroke echo → stays Idle.
        app.status.get_mut(&id).unwrap().last_input = now;
        app.detection_dirty.insert(id);
        app.detect_tick(now);
        assert_eq!(
            app.status.get(&id).unwrap().state,
            State::Idle,
            "typing echo must not read as agent working"
        );

        // No recent input: the same fresh output is the agent generating → Working.
        let later = now + std::time::Duration::from_millis(150);
        app.status.get_mut(&id).unwrap().last_activity = later;
        app.status.get_mut(&id).unwrap().last_input = now - std::time::Duration::from_secs(5);
        app.detection_dirty.insert(id);
        app.detect_tick(later);
        assert_eq!(
            app.status.get(&id).unwrap().state,
            State::Working,
            "output without recent typing is the agent working"
        );
    }

    // docs/07: switching to a tab whose panes resize repaints the agent; that
    // burst must not flip an idle pane to a lingering "working". Detection is
    // frozen for `RESIZE_GRACE` after a resize, then resumes normally.
    #[test]
    fn resize_grace_suppresses_a_transient_working_after_a_switch() {
        use crate::ui::theme::State;
        let _env = crate::persist::test_env("resize-grace");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let id = app.layout().focus;
        let t0 = std::time::Instant::now() + std::time::Duration::from_millis(200);

        // Recent output, no recent typing → the pane *wants* Working, but it was
        // just resized (switched into this tab), so it's inside the grace window.
        {
            let s = app.status.get_mut(&id).unwrap();
            s.state = State::Idle;
            s.last_activity = t0;
            s.last_input = t0 - std::time::Duration::from_secs(5);
            s.last_resize = Some(t0);
        }
        app.detection_dirty.insert(id);
        app.detect_tick(t0);
        assert_eq!(
            app.status.get(&id).unwrap().state,
            State::Idle,
            "a repaint right after a resize must not flip the pane to working"
        );

        // Past the grace window the same reading commits normally.
        let t1 = t0 + RESIZE_GRACE + std::time::Duration::from_millis(150);
        {
            let s = app.status.get_mut(&id).unwrap();
            s.last_activity = t1;
            s.last_input = t1 - std::time::Duration::from_secs(5);
        }
        app.detection_dirty.insert(id);
        app.detect_tick(t1);
        assert_eq!(
            app.status.get(&id).unwrap().state,
            State::Working,
            "once the grid settles, a genuinely active pane reads working again"
        );
    }

    // docs/29: config with no `sidebars` migrates to today's default layout.
    #[test]
    fn sidebars_migrate_from_legacy_width() {
        let cfg = crate::config::Config {
            sidebars: None,
            sidebar_width: 30,
            ..Default::default()
        };
        let s = cfg.sidebars();
        assert!(s.left.visible);
        assert_eq!(s.left.width, 30, "migration carries the legacy width");
        assert_eq!(s.left.docks, vec!["workspaces", "agents"]);
        assert!(!s.right.visible);
        assert!(s.right.docks.is_empty());
    }

    // docs/29 DOCK-3/4: move a built-in dock across sides, then push + retire a
    // module dock — the layout and cache track it.
    #[test]
    fn docks_move_and_module_dock_lifecycle() {
        let _env = crate::persist::test_env("docks");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        assert_eq!(
            app.sidebars.left.docks,
            vec![DockKind::Workspaces, DockKind::Agents]
        );
        assert!(app.sidebars.right.docks.is_empty());

        // Move Agents to the right sidebar (as the settings tab does).
        app.move_dock(&DockKind::Agents, Side::Right);
        assert_eq!(app.sidebars.left.docks, vec![DockKind::Workspaces]);
        assert_eq!(app.sidebars.right.docks, vec![DockKind::Agents]);
        assert!(
            app.config.sidebars.is_some(),
            "the move persisted to config"
        );

        // A module pushes a dock: it caches + auto-mounts on the requested side.
        let k = DockKind::Module("mod:status".into());
        app.push_module_dock(
            "mod:status",
            Some("Status".into()),
            Side::Right,
            vec![DockRow {
                text: "build ok".into(),
                dot: Some("done".into()),
                action: None,
                value: None,
                menu: Vec::new(),
            }],
        );
        assert_eq!(app.sidebars.side_of(&k), Some(Side::Right));
        assert_eq!(app.dock_label(&k), "Status");

        // Retiring the module removes its dock + cache.
        app.remove_module_docks(&["mod:status".into()]);
        assert_eq!(app.sidebars.side_of(&k), None);
        assert!(!app.module_docks.contains_key("mod:status"));
    }

    // docs/29: a sidebar holds at most MAX_DOCKS_PER_SIDE docks. A move onto a
    // full side is rejected (the dock keeps its spot), and an over-full config is
    // truncated to "off" on load.
    #[test]
    fn sidebar_dock_cap_is_enforced() {
        let _env = crate::persist::test_env("dock-cap");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();

        // Fill the left side to the cap: workspaces, agents, files.
        assert!(app.move_dock(&DockKind::Files, Side::Left));
        assert_eq!(app.sidebars.left.docks.len(), MAX_DOCKS_PER_SIDE);

        // A module dock now can't auto-mount onto the full left side — it stays off.
        let m = DockKind::Module("mod:x".into());
        app.push_module_dock("mod:x", None, Side::Left, Vec::new());
        assert_eq!(app.sidebars.side_of(&m), None, "no room → dock stays off");

        // Put it on the right, then try to move it onto the full left: rejected,
        // and it keeps its right-hand spot.
        assert!(app.move_dock(&m, Side::Right));
        assert!(
            !app.move_dock(&m, Side::Left),
            "moving onto a full side is rejected"
        );
        assert_eq!(
            app.sidebars.side_of(&m),
            Some(Side::Right),
            "the rejected dock keeps its original side"
        );
        assert_eq!(app.sidebars.left.docks.len(), MAX_DOCKS_PER_SIDE);

        // A config with more than the cap on one side truncates the overflow off.
        let cfg = crate::config::SidebarsConfig {
            left: crate::config::SideConfig {
                visible: true,
                width: 26,
                docks: vec![
                    "workspaces".into(),
                    "agents".into(),
                    "files".into(),
                    "mod:y".into(),
                ],
            },
            right: crate::config::SideConfig {
                visible: false,
                width: 26,
                docks: Vec::new(),
            },
        };
        let sidebars = Sidebars::from_config(&cfg);
        assert_eq!(
            sidebars.left.docks.len(),
            MAX_DOCKS_PER_SIDE,
            "load truncates the over-cap side"
        );
        assert!(
            !sidebars.left.has(&DockKind::Module("mod:y".into())),
            "the overflow dock is dropped to off"
        );
    }

    // Regression the cap could introduce: cycling (Enter) a dock whose next side is
    // full must skip to the next state, not get stuck. Left dock + full Right → Off.
    #[test]
    fn dock_cycle_skips_a_full_side() {
        use crate::app::settings::{LayoutRow, SettingsTab, SettingsUi};
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let _env = crate::persist::test_env("dock-cycle");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();

        // Left = [workspaces]; Right = [agents, files, mod:a] (full).
        app.move_dock(&DockKind::Agents, Side::Right);
        app.move_dock(&DockKind::Files, Side::Right);
        app.push_module_dock("mod:a", None, Side::Right, Vec::new());
        assert_eq!(app.sidebars.right.docks.len(), MAX_DOCKS_PER_SIDE);
        assert_eq!(app.sidebars.left.docks, vec![DockKind::Workspaces]);

        // Put the cursor on the Workspaces dock row and press Enter (cycle).
        let idx = app
            .layout_rows()
            .iter()
            .position(|r| matches!(r, LayoutRow::Dock(k) if *k == DockKind::Workspaces))
            .unwrap();
        app.settings = Some(SettingsUi {
            tab: SettingsTab::Layout,
            cursor: idx,
            prefix_candidate: None,
            layout_scroll: 0,
            capturing: false,
        });
        app.handle_settings_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        // Right was full, so the cycle skipped it and went straight to off — the
        // dock is not stuck on the left.
        assert_eq!(
            app.sidebars.side_of(&DockKind::Workspaces),
            None,
            "a Left dock cycles to off when the Right side is full"
        );
    }

    // docs/29 DOCK-2: with a dock on the right sidebar, it draws on the right and
    // the panes still keep at least 24 columns.
    #[test]
    fn right_sidebar_draws_and_guards_panes() {
        use ratatui::{backend::TestBackend, Terminal};
        let _env = crate::persist::test_env("rsb");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        app.move_dock(&DockKind::Agents, Side::Right);
        app.sidebars.right.visible = true;

        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();

        assert!(
            app.agents_area.x > 60,
            "agents dock drawn on the right half"
        );
        assert!(
            app.last_pane_area.width >= 24,
            "panes keep at least 24 columns"
        );
    }

    // docs/54 MC-1: Mission Control opens as a dashboard tab, lists the node's
    // live agents, renders them, and Enter jumps back to the agent's pane.
    #[test]
    /// Every dashboard must survive a hostile terminal. Mission Control lays its
    /// rows out in fixed-width columns, which is exactly the shape that panics on
    /// a subtract-overflow when the pane is narrower than the columns assume — so
    /// render each dashboard, and a file view, from tiny up to wide.
    fn dashboards_render_at_extreme_sizes() {
        use ratatui::{backend::TestBackend, Terminal};
        let _env = crate::persist::test_env("dash-sizes");
        for (w, h) in [
            (20u16, 6u16),
            (24, 8),
            (40, 10),
            (60, 5),
            (80, 24),
            (200, 60),
        ] {
            let (tx, _rx) = std::sync::mpsc::channel();
            let mut app = App::new(w.max(20), h.max(5), tx).unwrap();
            let focus = app.layout().focus;
            if let Some(st) = app.status.get_mut(&focus) {
                st.agent = "claude".into();
            }

            app.open_mission_control(0);
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            term.draw(|f| crate::ui::render(f, &mut app)).unwrap();

            app.open_orch_board();
            term.draw(|f| crate::ui::render(f, &mut app)).unwrap();

            // …and with a sidebar hidden, which changes every width downstream.
            app.sidebars.left.visible = false;
            term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
            app.sidebars.right.visible = false;
            term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        }
    }

    #[test]
    fn mission_control_lists_agents_and_jumps() {
        use crate::mission::MissionRow;
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::{backend::TestBackend, Terminal};
        let _env = crate::persist::test_env("mission");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        let agent_pane = app.layout().focus;
        let agent_tab = app.ws().active_tab;
        app.status.get_mut(&agent_pane).unwrap().agent = "claude".into();

        // Open Mission Control for the node (the right-click menu action).
        app.open_mission_control(0);
        assert!(app.active_is_mission(), "the mission tab is focused");
        assert_ne!(
            app.ws().active_tab,
            agent_tab,
            "it opened as a new dashboard tab"
        );

        // It lists the live agent.
        let rows = app.build_mission_rows();
        assert_eq!(rows.len(), 1, "one agent row");
        assert_eq!(rows[0].agent, "claude");
        assert_eq!(rows[0].row, MissionRow::Live(agent_pane));

        // It renders with the title and the agent visible.
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let buf = term.backend().buffer();
        let text: String = (0..buf.area.height)
            .flat_map(|r| {
                (0..buf.area.width)
                    .map(move |c| buf.cell((c, r)).map(|x| x.symbol()).unwrap_or(" "))
            })
            .collect();
        assert!(
            text.contains("Mission Control"),
            "the dashboard title shows"
        );
        assert!(text.contains("BETA"), "the dashboard beta badge shows");
        assert!(text.contains("claude"), "the agent row shows");

        // ⏎ jumps back to the agent's pane (leaving the dashboard).
        app.handle_mission_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!app.active_is_mission(), "jumped off the dashboard");
        assert_eq!(app.ws().active_tab, agent_tab, "landed on the agent's tab");
        assert_eq!(app.layout().focus, agent_pane, "focused the agent's pane");
    }

    #[test]
    fn mission_usage_refresh_is_demand_driven() {
        use ratatui::crossterm::event::{
            KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
        };
        use ratatui::{backend::TestBackend, Terminal};

        let _env = crate::persist::test_env("mission-demand-refresh");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        let normal_tab = app.ws().active_tab;
        app.open_mission_control(0);
        let mission_tab = app.ws().active_tab;
        assert!(app.mission_usage_requested, "opening requests one refresh");

        app.sync_mission_usage_visibility();
        app.mission_usage_requested = false; // simulate the worker consuming it
        app.sync_mission_usage_visibility();
        assert!(
            !app.mission_usage_requested,
            "remaining on Mission Control does not poll"
        );

        app.handle_mission_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert!(app.mission_usage_requested, "r requests a refresh");

        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let refresh = app
            .mission_refresh_rect
            .expect("wide dashboard exposes refresh button");
        app.mission_usage_requested = false;
        app.handle_event(crate::event::AppEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: refresh.x,
            row: refresh.y,
            modifiers: KeyModifiers::NONE,
        }));
        assert!(app.mission_usage_requested, "click requests a refresh");

        app.mission_usage_requested = false;
        app.focus_tab(normal_tab).unwrap();
        app.sync_mission_usage_visibility();
        app.focus_tab(mission_tab).unwrap();
        app.sync_mission_usage_visibility();
        assert!(
            app.mission_usage_requested,
            "returning to Mission Control requests one fresh snapshot"
        );
    }

    #[test]
    fn mission_usage_refreshes_when_switching_between_workspace_dashboards() {
        let _env = crate::persist::test_env("mission-workspace-refresh");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        let second = PaneId::alloc();
        app.workspaces.push(Workspace {
            id: crate::ids::public_id("workspace"),
            name: "beta".into(),
            cwd: PathBuf::from("/tmp/luvus-mission-beta"),
            branch: None,
            git_ahead_behind: None,
            worktree: None,
            tabs: vec![Tab::panes(TileLayout::new(second))],
            active_tab: 0,
            pinned: false,
        });

        app.open_mission_control(0);
        app.open_mission_control(1);
        app.sync_mission_usage_visibility();
        app.mission_usage_requested = false;

        app.active_ws = 0;
        app.sync_mission_usage_visibility();

        assert!(
            app.mission_usage_requested,
            "switching directly to another workspace dashboard requests fresh usage"
        );
    }

    #[test]
    fn mission_control_scope_tabs_include_every_workspace_without_row_clicks() {
        use crate::mission::{MissionRow, MissionScope};
        use ratatui::crossterm::event::{
            KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
        };
        use ratatui::{backend::TestBackend, Terminal};

        let _env = crate::persist::test_env("mission-scope");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        let first = app.layout().focus;
        app.workspaces[0].name = "alpha".into();
        app.status.get_mut(&first).unwrap().agent = "claude".into();

        let second = PaneId::alloc();
        let mut second_status = PaneStatus::new("codex".into());
        second_status.agent_session = Some(AgentSession {
            agent: "codex".into(),
            session_id: "beta-session".into(),
        });
        app.status.insert(second, second_status);
        app.workspaces.push(Workspace {
            id: crate::ids::public_id("workspace"),
            name: "beta".into(),
            cwd: PathBuf::from("/tmp/luvus-mission-beta"),
            branch: None,
            git_ahead_behind: None,
            worktree: None,
            tabs: vec![Tab::panes(TileLayout::new(second))],
            active_tab: 0,
            pinned: false,
        });

        app.open_mission_control(0);
        assert_eq!(app.mission_scope, MissionScope::Workspace);
        assert_eq!(app.build_mission_rows().len(), 1, "current workspace only");

        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let all = app
            .mission_scope_rects
            .iter()
            .find(|(scope, _)| *scope == MissionScope::All)
            .expect("all-workspaces scope tab is rendered")
            .1;
        app.handle_event(AppEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: all.x + 1,
            row: all.y,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(app.mission_scope, MissionScope::All);
        let rows = app.build_mission_rows();
        assert_eq!(rows.len(), 2, "both workspaces contribute agents");
        assert!(rows.iter().any(|row| {
            row.row == MissionRow::Live(second) && row.location.starts_with("beta · t1")
        }));

        app.handle_mission_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(
            app.mission_scope,
            MissionScope::Workspace,
            "Tab provides a keyboard equivalent"
        );
    }

    // docs/54 MC-2/MC-4: live rows carry cached usage (tokens/cost/context), and
    // the node's resumable on-disk sessions appear as their own rows.
    #[test]
    fn mission_control_shows_usage_and_resumables() {
        use crate::mission::{AgentUsage, MissionRow};
        let _env = crate::persist::test_env("mission-usage");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        let pane = app.layout().focus;
        let node_cwd = app.ws().cwd.clone();
        {
            let s = app.status.get_mut(&pane).unwrap();
            s.agent = "claude".into();
            s.agent_session = Some(AgentSession {
                agent: "claude".into(),
                session_id: "live-1".into(),
            });
        }
        // Seed the usage cache (what the async scan produces), keyed by agent and
        // session id.
        app.agent_usage.insert(
            crate::mission::UsageKey::new("claude", "live-1"),
            AgentUsage {
                model: "claude-opus-4-8".into(),
                tokens_in: 4000,
                tokens_out: 1200,
                cache: 300,
                context: Some(0.9),
                cost: Some(0.12),
            },
        );
        // A resumable session in the same node, with historical usage.
        app.resumable = vec![crate::agent::SessionInfo {
            agent: "claude".into(),
            session_id: "resume-1".into(),
            cwd: node_cwd,
            updated: std::time::SystemTime::now(),
        }];
        app.agent_usage.insert(
            crate::mission::UsageKey::new("claude", "resume-1"),
            AgentUsage {
                model: "claude-sonnet-4".into(),
                tokens_in: 1000,
                tokens_out: 200,
                cache: 0,
                context: Some(0.3),
                cost: Some(0.02),
            },
        );
        app.open_mission_control(0);
        let rows = app.build_mission_rows();

        let live = rows
            .iter()
            .find(|r| matches!(r.row, MissionRow::Live(_)))
            .expect("a live row");
        assert_eq!(
            live.usage.as_ref().and_then(|u| u.cost),
            Some(0.12),
            "the live row carries its cached cost"
        );
        let res = rows
            .iter()
            .find(|r| matches!(r.row, MissionRow::Session(_)))
            .expect("a resumable row");
        assert!(res.resumable, "flagged resumable");
        assert_eq!(res.agent, "claude");
        assert_eq!(
            res.usage.as_ref().and_then(|u| u.cost),
            Some(0.02),
            "the resumable row carries its historical cost"
        );

        // Render the dashboard and confirm the full command deck draws: agent
        // sessions, selected agent, status summary, and model-spend telemetry.
        {
            use ratatui::{backend::TestBackend, Terminal};
            let mut term = Terminal::new(TestBackend::new(140, 30)).unwrap();
            term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
            let buf = term.backend().buffer();
            let text: String = (0..buf.area.height)
                .flat_map(|r| {
                    (0..buf.area.width)
                        .map(move |c| buf.cell((c, r)).map(|x| x.symbol()).unwrap_or(" "))
                })
                .collect();
            assert!(text.contains("AGENT SESSIONS"), "the agent panel draws");
            assert!(text.contains("LOCATION"), "the table location header draws");
            assert!(text.contains("TOKENS"), "the table usage header draws");
            assert!(
                text.contains("SELECTED AGENT"),
                "the selected-agent panel draws"
            );
            assert!(
                text.contains("COST BY MODEL"),
                "the model-spend panel draws"
            );
            assert!(text.contains("AGENT STATUS"), "the state panel draws");
            assert!(text.contains('█'), "a telemetry bar draws");
        }

        // A narrow terminal falls back to a full-width, single-column agent list.
        {
            use ratatui::{backend::TestBackend, Terminal};
            let mut term = Terminal::new(TestBackend::new(60, 18)).unwrap();
            term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
            let buf = term.backend().buffer();
            let text: String = (0..buf.area.height)
                .flat_map(|r| {
                    (0..buf.area.width)
                        .map(move |c| buf.cell((c, r)).map(|x| x.symbol()).unwrap_or(" "))
                })
                .collect();
            assert!(text.contains("AGENT SESSIONS"), "compact agent panel draws");
        }

        // Plain clicks are intentionally inert: opening a pane is an explicit
        // keyboard action through Enter, not an accidental dashboard click.
        let active_tab = app.ws().active_tab;
        let cursor = app.mission_cursor;
        app.handle_event(crate::event::AppEvent::Mouse(
            ratatui::crossterm::event::MouseEvent {
                kind: ratatui::crossterm::event::MouseEventKind::Down(
                    ratatui::crossterm::event::MouseButton::Left,
                ),
                column: app.mission_area.x.saturating_add(2),
                row: app.mission_area.y.saturating_add(7),
                modifiers: KeyModifiers::NONE,
            },
        ));
        assert!(app.active_is_mission(), "click stays in Mission Control");
        assert_eq!(
            app.ws().active_tab,
            active_tab,
            "click does not open a pane"
        );
        assert_eq!(
            app.mission_cursor, cursor,
            "click does not change selection"
        );

        // The detail overlay opens on `o` and closes on esc. (`render` publishes
        // `mission_rows`, so it's set now.)
        app.handle_mission_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE));
        assert!(app.mission_detail.is_some(), "detail overlay opened");
        app.handle_mission_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.mission_detail.is_none(), "detail overlay closed");
    }

    // docs/54: a blocked agent shows what it's waiting on, and the inline answer
    // input captures typing (including keys that are otherwise row shortcuts).
    #[test]
    fn mission_control_blocked_hint_and_answer_input() {
        use crate::ui::theme::State;
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let _env = crate::persist::test_env("mission-answer");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 40, tx).unwrap();
        let pane = app.layout().focus;
        {
            let s = app.status.get_mut(&pane).unwrap();
            s.agent = "claude".into();
            s.state = State::Blocked;
            s.blocked_hint = Some("Do you want to proceed? (y/n)".into());
        }
        app.open_mission_control(0);
        app.mission_rows = app.build_mission_rows();
        assert_eq!(
            app.mission_rows[0].blocked_hint.as_deref(),
            Some("Do you want to proceed? (y/n)"),
            "the row carries the blocking prompt"
        );

        // Render the tab with both overlays open + a budget set (exercises the
        // blocked-hint row, the detail modal, the answer input, and the header).
        {
            use ratatui::{backend::TestBackend, Terminal};
            app.config.mission_budget = Some(1.0);
            app.mission_detail = Some(0);
            app.mission_answer = Some("hi".into());
            let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
            term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
            app.mission_detail = None;
            app.mission_answer = None;
        }

        let key = |c| KeyEvent::new(c, KeyModifiers::NONE);
        // `a` opens the inline answer input.
        app.mission_cursor = 0;
        app.handle_mission_key(key(KeyCode::Char('a')));
        assert_eq!(
            app.mission_answer.as_deref(),
            Some(""),
            "answer input opened"
        );
        // While answering, ordinary shortcut keys are typed, not acted on: `q`
        // types a q rather than closing the tab.
        app.handle_mission_key(key(KeyCode::Char('q')));
        app.handle_mission_key(key(KeyCode::Char('!')));
        assert_eq!(app.mission_answer.as_deref(), Some("q!"));
        assert!(app.active_is_mission(), "still on the mission tab");
        // Enter sends it and closes the input.
        app.handle_mission_key(key(KeyCode::Enter));
        assert!(app.mission_answer.is_none(), "answer sent, input closed");

        // `x` closes the selected live agent's pane.
        app.mission_rows = app.build_mission_rows();
        app.handle_mission_key(key(KeyCode::Char('x')));
        assert!(!app.panes.contains_key(&pane), "x closed the agent's pane");
    }

    // The Shell picker is Windows-only (control row 5 doesn't exist elsewhere).
    #[cfg(windows)]
    #[test]
    fn settings_shell_choice_cycles_and_persists() {
        use ratatui::backend::TestBackend;
        use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        use ratatui::Terminal;

        let _env = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("luvus-shell-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("LUVUS_HOME", &tmp);

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        app.open_settings();
        app.handle_event(AppEvent::Key(KeyEvent::new(
            KeyCode::Char('2'),
            KeyModifiers::NONE,
        ))); // Layout
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();

        assert_eq!(app.config.shell, "default");
        // The Shell row (control index 5) cycles forward on click.
        let row = app
            .settings_ctl_rects
            .iter()
            .find(|(i, _)| *i == 5)
            .unwrap()
            .1;
        app.handle_event(AppEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: row.x + 2,
            row: row.y,
            modifiers: KeyModifiers::NONE,
        }));
        assert_ne!(
            app.config.shell, "default",
            "clicking the Shell row cycles it"
        );
        // …and the choice is persisted.
        assert_eq!(crate::config::load().shell, app.config.shell);

        std::env::remove_var("LUVUS_HOME");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn blocked_transition_plays_sound_when_enabled() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let id = app.layout().focus;
        // Drive the pane's screen to a permission prompt so detection sees
        // Blocked. Newlines push it to the bottom rows that detection scans.
        if let Some(p) = app.panes.get(&id) {
            if let Ok(mut e) = p.engine.lock() {
                let mut buf = vec![b'\n'; 30];
                buf.extend_from_slice(b"Do you want to proceed? (y/n) ");
                e.advance(&buf);
            }
        }
        // The chime only rings for agent panes.
        app.status.get_mut(&id).unwrap().agent_session = Some(AgentSession {
            agent: "claude".into(),
            session_id: "s".into(),
        });

        // Successive ticks must each clear the detection cadence gate (~100ms),
        // so drive them with explicitly advancing instants.
        let t0 = std::time::Instant::now();

        // Off by default: the same transition stays silent.
        app.status.get_mut(&id).unwrap().state = State::Idle;
        app.detect_tick(t0);
        assert!(!app.pending_sound, "sound on blocked is off by default");

        // Enabled → a transition to Blocked rings once…
        app.config.notifications.sound_on_blocked = true;
        app.status.get_mut(&id).unwrap().state = State::Idle; // re-run the transition
        app.detect_tick(t0 + Duration::from_millis(200));
        assert!(app.pending_sound, "blocked transition rings when enabled");

        // …and is disarmed: a flap back into Blocked doesn't ring again until
        // the user looks at the pane (focus re-arms; this pane is focused, so
        // simulate the unfocused case by moving focus away).
        app.pending_sound = false;
        let bogus = PaneId::alloc();
        app.layout_mut().focus = bogus; // unfocused → no auto re-arm
        app.status.get_mut(&id).unwrap().state = State::Idle;
        app.detect_tick(t0 + Duration::from_millis(400));
        assert!(!app.pending_sound, "an ignored prompt doesn't ring twice");
    }

    // A bursty/streaming agent has long pauses *within* one turn. The debounce
    // (QUIET_DWELL) must hold the status at Working through those pauses and
    // only commit Done — and chime — on sustained quiet, once per real finish.
    #[test]
    fn done_chime_debounced_and_rings_per_finish() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.config.notifications.sound_on_done = true;
        let id = app.layout().focus;
        // The chime only rings for agent panes.
        app.status.get_mut(&id).unwrap().agent_session = Some(AgentSession {
            agent: "claude".into(),
            session_id: "s".into(),
        });
        // Treat the pane as unfocused so it can reach the Done state.
        let bogus = PaneId::alloc();
        app.layout_mut().focus = bogus;

        let t0 = std::time::Instant::now();
        // Drive the state through the *real* detection path. An agent is Working
        // only while a marker is on screen — raw output does not imply work (see
        // `repaint_without_the_agent_name_stays_idle`) — so paint and clear a
        // spinner rather than just poking `last_activity`.
        // Newlines scroll the previous marker away and land the new text in the
        // bottom rows, which is the region detection actually scans.
        let paint = |app: &mut App, text: &str| {
            {
                if let Some(p) = app.panes.get(&id) {
                    if let Ok(mut e) = p.engine.lock() {
                        let mut buf = vec![b'\n'; 30];
                        buf.extend_from_slice(text.as_bytes());
                        e.advance(&buf);
                    }
                }
            }
            // Production output reaches detection through `PtyData`, which marks
            // the pane dirty. This direct VT mutation must model the same event.
            app.detection_dirty.insert(id);
        };
        let go_working = |app: &mut App, base: std::time::Instant| {
            paint(app, "⠋ Thinking… (esc to interrupt)\r\n");
            app.status.get_mut(&id).unwrap().last_activity = base;
        };
        // Marker gone + stale output → the pane reads raw-Idle.
        let go_quiet = |app: &mut App, base: std::time::Instant| {
            paint(app, "> \r\n");
            app.status.get_mut(&id).unwrap().last_activity =
                base - ACTIVITY_WINDOW - Duration::from_millis(50);
        };
        let state = |app: &App| app.status.get(&id).unwrap().state;

        // Prime: the pane was Working.
        {
            let s = app.status.get_mut(&id).unwrap();
            s.state = State::Working;
            s.prev_working = true;
        }

        // (1) A pause shorter than the dwell must NOT flip to Done — the whole
        // point: status stays steady through a streaming gap, and no bell.
        go_quiet(&mut app, t0);
        app.detect_tick(t0); // candidate=Done, but not yet committed
        app.detect_tick(t0 + Duration::from_millis(500));
        assert_eq!(state(&app), State::Working, "a short pause stays Working");
        assert!(!app.pending_sound, "a short pause does not chime");

        // (2) Sustained quiet past the dwell → Done, chiming.
        app.detect_tick(t0 + QUIET_DWELL + Duration::from_millis(100));
        assert_eq!(state(&app), State::Done, "sustained quiet commits Done");
        assert!(app.pending_sound, "a genuine completion chimes");

        // (3) Work again, then complete again → a second genuine finish chimes
        // too (the chime is per finish; the debounce is what stops mid-turn
        // pauses from ringing).
        app.pending_sound = false;
        let t1 = t0 + QUIET_DWELL + Duration::from_millis(300);
        go_working(&mut app, t1); // spinner back on screen → Working
        app.detect_tick(t1); // commits Working instantly
        assert_eq!(
            state(&app),
            State::Working,
            "activity returns to Working at once"
        );
        go_quiet(&mut app, t1);
        app.detect_tick(t1 + QUIET_DWELL + Duration::from_millis(100)); // arm candidate=Done
        app.detect_tick(t1 + 2 * QUIET_DWELL + Duration::from_millis(200)); // commit Done
        assert_eq!(
            state(&app),
            State::Done,
            "second completion still reaches Done"
        );
        assert!(app.pending_sound, "each real finish chimes");
    }

    // Keyboard scroll mode: Shift+↑ enters, plain keys navigate the scrollback
    // (numbers jump, j/k lines), and `q`/`0` return to live + exit — no prefix.
    #[test]
    fn scroll_mode_navigates_and_exits() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let id = app.layout().focus;
        // Give the focused pane real scrollback history.
        if let Some(p) = app.panes.get(&id) {
            if let Ok(mut e) = p.engine.lock() {
                for i in 0..200 {
                    e.advance(format!("line {i}\r\n").as_bytes());
                }
            }
        }
        let off = |app: &App| app.panes.get(&id).unwrap().scroll_state().0;
        let plain = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        let send = |app: &mut App, k: KeyEvent| {
            app.handle_event(AppEvent::Key(k));
        };

        assert!(app.scroll_pane.is_none());
        // Shift+↑ enters scroll mode and scrolls up — no Ctrl+Space needed.
        send(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT));
        assert_eq!(app.scroll_pane, Some(id), "Shift+Up enters scroll mode");
        assert!(off(&app) > 0, "and scrolls up into history");

        // `1` jumps to the oldest, `9` near the newest.
        send(&mut app, plain('1'));
        let top = off(&app);
        assert!(top > 3, "1 jumps to the top of history: {top}");
        send(&mut app, plain('9'));
        assert!(off(&app) < top, "9 is nearer the live bottom");

        // `k`/`j` move one line.
        let before = off(&app);
        send(&mut app, plain('k'));
        assert_eq!(off(&app), before + 1, "k scrolls up a line");
        send(&mut app, plain('j'));
        assert_eq!(off(&app), before, "j scrolls down a line");

        // `q` returns to live and leaves the mode.
        send(&mut app, plain('q'));
        assert!(app.scroll_pane.is_none(), "q exits scroll mode");
        assert_eq!(off(&app), 0, "and snaps back to live");

        // `0` also returns to live and exits.
        send(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT));
        assert_eq!(app.scroll_pane, Some(id));
        send(&mut app, plain('0'));
        assert!(app.scroll_pane.is_none(), "0 returns to live and exits");
        assert_eq!(off(&app), 0);
    }

    #[test]
    fn keyboard_copy_mode_yanks_history_and_cancel_restores_its_viewport() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let id = app.layout().focus;
        if let Some(pane) = app.panes.get(&id) {
            if let Ok(mut engine) = pane.engine.lock() {
                for i in 0..80 {
                    engine.advance(format!("line {i}\r\n").as_bytes());
                }
            }
        }
        let send = |app: &mut App, code, modifiers| {
            app.handle_event(AppEvent::Key(KeyEvent::new(code, modifiers)));
        };
        let offset = |app: &App| app.panes.get(&id).unwrap().scroll_state().0;

        send(&mut app, KeyCode::Char(' '), KeyModifiers::CONTROL);
        send(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
        assert!(
            app.copy_mode.is_some(),
            "the default prefix then y enters keyboard copy mode"
        );
        send(&mut app, KeyCode::Char('g'), KeyModifiers::NONE);
        send(&mut app, KeyCode::Char('v'), KeyModifiers::NONE);
        send(&mut app, KeyCode::Char('l'), KeyModifiers::NONE);
        send(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
        assert_eq!(app.pending_clipboard.as_deref(), Some("li"));
        assert!(app.copy_mode.is_none(), "y exits copy mode");
        assert_eq!(offset(&app), 0, "copy returns to live output");

        app.panes.get(&id).unwrap().scroll(8);
        let saved = offset(&app);
        send(&mut app, KeyCode::Char(' '), KeyModifiers::CONTROL);
        send(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
        send(&mut app, KeyCode::Char('j'), KeyModifiers::NONE);
        send(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(app.copy_mode.is_none(), "q cancels copy mode");
        assert_eq!(offset(&app), saved, "cancel restores the prior viewport");
    }

    #[test]
    fn keyboard_copy_mode_navigates_and_yanks_wide_cells() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(40, 8, tx).unwrap();
        let pane = app.layout().focus;
        app.panes
            .get(&pane)
            .expect("pane")
            .engine
            .lock()
            .expect("engine")
            .advance("\x1b[H\x1b[2J你好，hello.".as_bytes());
        let mut target_row = None;
        app.panes
            .get(&pane)
            .expect("pane")
            .for_each_retained_row(&mut |row, _, _, text| {
                if text == "你好，hello." {
                    target_row = Some(row);
                }
            });
        let row = target_row.expect("fixture row");
        app.copy_mode = Some(CopyMode {
            pane,
            anchor: (row, 0),
            cursor: (row, 0),
            saved_scroll: 0,
        });

        for _ in 0..3 {
            app.handle_event(AppEvent::Key(KeyEvent::new(
                KeyCode::Char('l'),
                KeyModifiers::NONE,
            )));
        }
        assert_eq!(app.copy_mode.expect("copy mode").cursor, (row, 3));
        app.handle_event(AppEvent::Key(KeyEvent::new(
            KeyCode::Char('y'),
            KeyModifiers::NONE,
        )));

        assert_eq!(app.pending_clipboard.as_deref(), Some("你好"));
    }

    #[test]
    fn keyboard_copy_word_navigation_uses_visual_columns() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(40, 8, tx).unwrap();
        let pane = app.layout().focus;
        app.panes
            .get(&pane)
            .expect("pane")
            .engine
            .lock()
            .expect("engine")
            .advance("\x1b[H\x1b[2J你好 world".as_bytes());
        let mut target_row = None;
        app.panes
            .get(&pane)
            .expect("pane")
            .for_each_retained_row(&mut |row, _, _, text| {
                if text == "你好 world" {
                    target_row = Some(row);
                }
            });
        let row = target_row.expect("fixture row");
        app.copy_mode = Some(CopyMode {
            pane,
            anchor: (row, 0),
            cursor: (row, 0),
            saved_scroll: 0,
        });

        let send = |app: &mut App, character| {
            app.handle_event(AppEvent::Key(KeyEvent::new(
                KeyCode::Char(character),
                KeyModifiers::NONE,
            )));
        };
        send(&mut app, 'w');
        assert_eq!(app.copy_mode.expect("copy mode").cursor, (row, 5));
        send(&mut app, 'B');
        assert_eq!(app.copy_mode.expect("copy mode").cursor, (row, 0));
        send(&mut app, '$');
        assert_eq!(app.copy_mode.expect("copy mode").cursor, (row, 9));
    }

    #[test]
    fn keyboard_copy_trims_a_codex_transcript_gutter() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(40, 8, tx).unwrap();
        let pane = app.layout().focus;
        app.status.get_mut(&pane).expect("pane status").agent = "codex".into();
        app.panes
            .get(&pane)
            .expect("pane")
            .engine
            .lock()
            .expect("engine")
            .advance(b"\x1b[H\x1b[2J Hello\r\n world");
        app.copy_mode = Some(CopyMode {
            pane,
            anchor: (1, 0),
            cursor: (2, 5),
            saved_scroll: 0,
        });

        app.finish_copy_mode();

        assert_eq!(app.pending_clipboard.as_deref(), Some("Hello\nworld"));
    }

    #[test]
    fn shift_v_never_enters_copy_mode() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(40, 8, tx).unwrap();
        let pane = app.layout().focus;
        app.panes
            .get(&pane)
            .expect("pane")
            .engine
            .lock()
            .expect("engine")
            .advance(b"terminal input");

        assert!(
            !app.handle_event(AppEvent::Key(KeyEvent::new(
                KeyCode::Char('V'),
                KeyModifiers::SHIFT,
            ))),
            "forwarded terminal input waits for child output to repaint"
        );
        assert!(
            app.copy_mode.is_none(),
            "uppercase V never enters copy mode"
        );
    }

    #[test]
    fn prefix_y_starts_copy_mode() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(40, 8, tx).unwrap();
        let pane = app.layout().focus;
        app.status.get_mut(&pane).expect("pane status").agent = "codex".into();
        app.panes
            .get(&pane)
            .expect("pane")
            .engine
            .lock()
            .expect("engine")
            .advance(b"build finished successfully");

        assert!(app.handle_event(AppEvent::Key(KeyEvent::new(
            KeyCode::Char(' '),
            KeyModifiers::CONTROL,
        ))));
        assert!(
            app.handle_event(AppEvent::Key(KeyEvent::new(
                KeyCode::Char('y'),
                KeyModifiers::NONE,
            ))),
            "the default prefix then y starts copy mode over a Codex transcript"
        );
        assert!(app.copy_mode.is_some());
    }
}

#[cfg(test)]
mod cwd_test {
    use super::*;

    #[test]
    fn pane_cwd_follows_cd_without_moving_its_workspace() {
        let _env = crate::persist::test_env("pane-cwd-follows-cd");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let id = app.layout().focus;
        let workspace_cwd = app.ws().cwd.clone();
        let workspace_name = app.ws().name.clone();
        let deadline = Instant::now() + Duration::from_secs(8);

        // Poll a real child process up to a deadline. Repeating the idempotent
        // command handles shells that have not finished startup yet without a
        // fixed readiness sleep.
        let mut got = String::new();
        while Instant::now() < deadline {
            app.panes.get(&id).unwrap().send(b"cd /tmp\r");
            std::thread::sleep(Duration::from_millis(100));
            app.refresh_cwds();
            got = app.panes.get(&id).unwrap().cwd.display().to_string();
            if got.contains("tmp") {
                break;
            }
        }
        assert!(got.contains("tmp"), "cwd did not follow cd: got '{got}'");
        assert_eq!(
            app.ws().cwd,
            workspace_cwd,
            "cd changes the pane cwd, not its static workspace root"
        );
        assert_eq!(
            app.ws().name,
            workspace_name,
            "cd does not rename the static workspace"
        );
    }
}

#[cfg(test)]
mod dock_fn_check {
    use super::*;
    #[test]
    fn off_unmounts_and_stays_in_registry() {
        let _env = crate::persist::test_env("offscroll");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(96, 30, tx).unwrap();
        assert_eq!(app.sidebars.side_of(&DockKind::Agents), Some(Side::Left));
        app.unmount_dock(&DockKind::Agents); // the [Off] action
        assert_eq!(
            app.sidebars.side_of(&DockKind::Agents),
            None,
            "Off unmounts"
        );
        assert!(
            app.available_docks().contains(&DockKind::Agents),
            "still in the registry to re-place"
        );
    }
    #[test]
    fn layout_tab_scrolls_to_cursor() {
        use ratatui::{backend::TestBackend, Terminal};
        let _env = crate::persist::test_env("scroll");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(96, 30, tx).unwrap();
        app.open_settings();
        let n = app.settings_rows(crate::app::SettingsTab::Layout);
        if let Some(u) = app.settings.as_mut() {
            u.tab = crate::app::SettingsTab::Layout;
            u.cursor = n - 1; // last row
        }
        let mut term = Terminal::new(TestBackend::new(96, 16)).unwrap(); // short → must scroll
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        assert!(
            app.settings_ctl_rects.iter().any(|(i, _)| *i == n - 1),
            "last Layout row visible after scrolling to it"
        );
    }
}
