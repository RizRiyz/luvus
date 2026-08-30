//! Session persistence (M5): snapshot the workspace/tab/pane tree to
//! `~/.config/luvus/session.json` and restore it on launch. Captures structure
//! + cwds only — restore re-spawns shells. See docs/09.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::app::App;
use crate::ids::PaneId;
use crate::layout::LayoutTree;

const SNAPSHOT_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub version: u32,
    pub active_ws: usize,
    pub workspaces: Vec<WsSnap>,
}

#[derive(Serialize, Deserialize)]
pub struct WsSnap {
    #[serde(default = "new_workspace_id")]
    pub id: String,
    pub name: String,
    pub cwd: PathBuf,
    pub active_tab: usize,
    pub tabs: Vec<TabSnap>,
    /// Pinned to the top of the WORKSPACES list (right-click → Pin).
    #[serde(default)]
    pub pinned: bool,
}

#[derive(Serialize, Deserialize)]
pub struct TabSnap {
    #[serde(default = "new_tab_id")]
    pub id: String,
    pub tree: LayoutTree,
    pub focus: u32,
    /// (raw pane id at save time → its cwd/command).
    pub panes: Vec<(u32, PaneSnap)>,
    /// A git tab (docs/17) — restored as the dashboard (no panes), re-fetched.
    #[serde(default)]
    pub git: bool,
    /// The orchestration board (docs/22, ORCH-7) — restored as the placeholder
    /// dashboard tab; its data lives in the shared `orch.json` ledger.
    #[serde(default)]
    pub orch: bool,
    /// The Mission Control dashboard (docs/54) — restored as a placeholder tab; its
    /// data (agents/usage) is re-derived, nothing is stored.
    #[serde(default)]
    pub mission: bool,
    /// User-chosen tab name (docs/28); `None` → the tab shows its number.
    #[serde(default)]
    pub name: Option<String>,
}

fn new_workspace_id() -> String {
    crate::ids::public_id("workspace")
}

fn new_tab_id() -> String {
    crate::ids::public_id("tab")
}

#[derive(Serialize, Deserialize)]
pub struct PaneSnap {
    pub cwd: PathBuf,
    pub command: String,
    /// The pane's live name (`pane name` / `agent name`), so the alias and its
    /// title survive a restart. Re-attached to the pane's new id on restore.
    #[serde(default)]
    pub name: Option<String>,
    /// (agent, session_id) for native resume, if reported.
    #[serde(default)]
    pub agent_session: Option<(String, String)>,
    /// The launch flags the agent pane was started with (argv after the agent
    /// token, docs/62), replayed after the resume reference on restore. Session
    /// selection flags are filtered at replay, so the stored list stays faithful.
    #[serde(default)]
    pub agent_launch: Option<Vec<String>>,
    /// The visible screen as ANSI, replayed on restore.
    #[serde(default)]
    pub screen: Option<String>,
    /// (module_id, entrypoint) for a module pane (MOD-2), re-spawned on restore.
    #[serde(default)]
    pub module: Option<(String, String)>,
    /// A native file **view** leaf (docs/38 FILE-3): the file it shows. When set,
    /// restore rebuilds the view (re-reads the file) instead of spawning a shell.
    #[serde(default)]
    pub file: Option<PathBuf>,
    /// A native DIFF view specification. Patch content is always re-fetched.
    #[serde(default)]
    pub diff: Option<DiffSnap>,
    /// An explicit derived document preview. Source is always re-read.
    #[serde(default)]
    pub preview: Option<PreviewSnap>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PreviewSnap {
    pub path: PathBuf,
    pub kind: crate::files::preview::PreviewKind,
    #[serde(default)]
    pub scroll: usize,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct DiffSnap {
    pub root: PathBuf,
    pub key: crate::diff::DiffKey,
    pub status: crate::diff::DiffFileStatus,
    #[serde(default)]
    pub preference: crate::diff::DiffLayoutPreference,
    #[serde(default)]
    pub scroll: usize,
    #[serde(default)]
    pub selected: usize,
    #[serde(default)]
    pub selected_side: crate::diff::DiffSide,
    #[serde(default)]
    pub horizontal: usize,
    #[serde(default)]
    pub wrap: bool,
    #[serde(default = "default_diff_snap_context_lines")]
    pub context_lines: u16,
    #[serde(default = "default_diff_snap_line_numbers")]
    pub show_line_numbers: bool,
}

fn default_diff_snap_context_lines() -> u16 {
    crate::config::Config::default().diff_context_lines()
}

fn default_diff_snap_line_numbers() -> bool {
    crate::config::Config::default()
        .layout
        .diff_show_line_numbers
}

/// Serializes tests that mutate the global `$LUVUS_HOME` env + config files, so
/// they don't race on each other's config / registry I/O. Lock it for the whole
/// test body. Shared across modules (`app`, `module`, …).
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// RAII test isolation: locks [`TEST_ENV_LOCK`] **and** points `$LUVUS_HOME` at a
/// fresh empty dir, so the test reads/writes only default, isolated config — never
/// racing another test's keybinding/theme overrides (`$LUVUS_HOME` is process-global,
/// so the lock alone isn't enough; a parallel `App::new` would still read whatever
/// dir a mutating test had set). Restores `$LUVUS_HOME` + removes the dir on drop.
/// Bind it for the whole test body: `let _env = test_env("name");`.
#[cfg(test)]
pub(crate) struct TestEnv {
    _guard: std::sync::MutexGuard<'static, ()>,
    prev: Option<std::ffi::OsString>,
    prev_session: Option<std::ffi::OsString>,
    prev_socket: Option<std::ffi::OsString>,
    dir: PathBuf,
}

#[cfg(test)]
impl Drop for TestEnv {
    fn drop(&mut self) {
        match &self.prev {
            Some(p) => std::env::set_var("LUVUS_HOME", p),
            None => std::env::remove_var("LUVUS_HOME"),
        }
        match &self.prev_session {
            Some(value) => std::env::set_var(crate::session::SESSION_ENV_VAR, value),
            None => std::env::remove_var(crate::session::SESSION_ENV_VAR),
        }
        match &self.prev_socket {
            Some(value) => std::env::set_var("LUVUS_SOCKET_PATH", value),
            None => std::env::remove_var("LUVUS_SOCKET_PATH"),
        }
        crate::session::clear_explicit_for_test();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
pub(crate) fn test_env(tag: &str) -> TestEnv {
    let guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var_os("LUVUS_HOME");
    let prev_session = std::env::var_os(crate::session::SESSION_ENV_VAR);
    let prev_socket = std::env::var_os("LUVUS_SOCKET_PATH");
    let dir = std::env::temp_dir().join(format!("luvus-test-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::env::set_var("LUVUS_HOME", &dir);
    std::env::remove_var(crate::session::SESSION_ENV_VAR);
    std::env::remove_var("LUVUS_SOCKET_PATH");
    crate::session::clear_explicit_for_test();
    TestEnv {
        _guard: guard,
        prev,
        prev_session,
        prev_socket,
        dir,
    }
}

/// `~/.luvus/` (or `~/.luvus-dev/` in debug builds). Override with `$LUVUS_HOME`.
pub fn config_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("LUVUS_HOME") {
        return PathBuf::from(p);
    }
    let home = crate::platform::home_dir().unwrap_or_default();
    let name = if cfg!(debug_assertions) {
        ".luvus-dev"
    } else {
        ".luvus"
    };
    home.join(name)
}

/// Create the state dir if needed and, on Unix, keep it owner-only (`0700`).
/// The control sockets inside grant full command execution as the user, and
/// some BSDs ignore permissions on a socket *file* — the directory mode is the
/// reliable barrier, so don't leave it to the umask. Guarded against a
/// pathological `$LUVUS_HOME=$HOME` (never chmod the home dir itself).
pub fn ensure_config_dir() -> PathBuf {
    let dir = config_dir();
    ensure_private_dir(&dir);
    dir
}

/// Selected server runtime directory. The default session remains rooted at
/// `config_dir`; named sessions live under `config_dir/sessions/<name>`.
pub fn session_dir() -> PathBuf {
    crate::session::active_dir()
}

/// Records the live server PID so `server stop` can kill an unresponsive process
/// without waiting on IPC. Dropped on a clean server exit; a crash leaves the
/// file, and the stopper still checks the PID is a Luvus process we own.
pub struct ServerPidFile;

impl ServerPidFile {
    pub fn claim() -> Self {
        let pid = std::process::id();
        let mut body = pid.to_string();
        if let Some(marker) = crate::platform::process_start_marker(pid) {
            body.push(' ');
            body.push_str(&marker);
        }
        let _ = fs::write(session_dir().join("server.pid"), body);
        Self
    }

    pub fn read() -> Option<u32> {
        let text = fs::read_to_string(session_dir().join("server.pid")).ok()?;
        let mut parts = text.split_whitespace();
        let pid: u32 = parts.next()?.parse().ok()?;
        if let Some(recorded) = parts.next() {
            let live = crate::platform::process_start_marker(pid)?;
            if live != recorded {
                return None;
            }
        }
        Some(pid)
    }
}

impl Drop for ServerPidFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(session_dir().join("server.pid"));
    }
}

#[cfg(test)]
mod server_pid_file_tests {
    use super::*;

    #[test]
    fn server_pid_file_reads_the_live_process() {
        let _env = test_env("server-pid-live");
        ensure_session_dir();
        let claimed = ServerPidFile::claim();
        assert_eq!(ServerPidFile::read(), Some(std::process::id()));
        drop(claimed);
        assert_eq!(ServerPidFile::read(), None);
    }

    #[test]
    fn server_pid_file_rejects_a_mismatched_start_marker() {
        let _env = test_env("server-pid-mismatch");
        ensure_session_dir();
        let claimed = ServerPidFile::claim();
        let path = session_dir().join("server.pid");
        fs::write(&path, format!("{} not-a-live-marker", std::process::id())).unwrap();
        assert_eq!(ServerPidFile::read(), None);
        drop(claimed);
    }
}

/// Create the selected runtime directory with the same owner-only protection as
/// the global root. This is the startup-lock namespace for one server only.
pub fn ensure_session_dir() -> PathBuf {
    let dir = session_dir();
    ensure_private_dir(&dir);
    #[cfg(unix)]
    for socket in [socket_path(), client_socket_path()] {
        if let Some(parent) = socket.parent().filter(|parent| *parent != dir) {
            ensure_private_dir(parent);
        }
    }
    dir
}

/// Create and validate the selected server runtime directory. Unlike the
/// best-effort helpers used by ordinary config reads, server startup fails
/// closed because its sockets grant command execution as the current user.
pub fn ensure_server_session_dir() -> std::io::Result<PathBuf> {
    let dir = session_dir();
    ensure_private_server_dir(&dir)?;
    #[cfg(unix)]
    for socket in [socket_path(), client_socket_path()] {
        if let Some(parent) = socket.parent().filter(|parent| *parent != dir) {
            ensure_private_server_dir(parent)?;
        }
    }
    Ok(dir)
}

fn ensure_private_server_dir(dir: &std::path::Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if Some(dir) == crate::platform::home_dir().as_deref() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Luvus server state directory cannot be the home directory",
            ));
        }
        let metadata = fs::symlink_metadata(dir)?;
        // SAFETY: `geteuid` has no preconditions.
        let current_uid = unsafe { libc::geteuid() };
        if !metadata.file_type().is_dir() || metadata.uid() != current_uid {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "Luvus server state must be a real directory owned by the current user: {}",
                    dir.display()
                ),
            ));
        }
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        if fs::symlink_metadata(dir)?.mode() & 0o777 != 0o700 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("could not protect Luvus server state: {}", dir.display()),
            ));
        }
    }
    Ok(())
}

fn ensure_private_dir(dir: &std::path::Path) {
    let _ = fs::create_dir_all(dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if Some(dir) != crate::platform::home_dir().as_deref() {
            let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
        }
    }
}

fn session_path() -> PathBuf {
    session_dir().join("session.json")
}

/// User-editable agent-detection manifests (docs/07). `~/.luvus/manifests/`.
pub fn manifests_dir() -> PathBuf {
    config_dir().join("manifests")
}

/// Opt-in skill ownership state and migration marker. The canonical skill is
/// bundled in the binary; enabled copies live in agent-native locations.
pub fn skills_dir() -> PathBuf {
    config_dir().join("skills")
}

/// Create the manifests dir if it doesn't exist and drop an annotated example
/// the first time, so the feature is discoverable. Best-effort; never fatal.
pub fn ensure_manifests_dir() -> PathBuf {
    let dir = manifests_dir();
    if !dir.exists() {
        let _ = fs::create_dir_all(&dir);
        let _ = fs::write(dir.join("example.toml.txt"), MANIFEST_EXAMPLE);
    }
    dir
}

/// Sample manifest shipped into `~/.luvus/manifests/` on first run. The `.txt`
/// suffix keeps it from being loaded; copy it to `<agent>.toml` and edit.
const MANIFEST_EXAMPLE: &str = "\
# luvus agent-detection manifest (docs/07). Copy to `<agent>.toml` and edit --
# one file per agent keeps things findable. Every *.toml here merges into
# luvus's built-in detection; rules are merged by priority (highest wins), so a
# higher-priority rule overrides a built-in one for the same agent.
#
# A manifest controls two separate things:
#   [identity]  -- how luvus decides *which agent* a pane is running
#   [[rule]]    -- how luvus decides *what state* that agent is in

# Which agent this file applies to. `generic` (default) means all agents, and
# is only valid for [[rule]] -- identity needs a specific agent.
agent = \"claude\"

# ── identity (optional) ──────────────────────────────────────────────────────
# Patterns are matched as whole words, so `amp` no longer matches inside
# \"example\" and `.kiro/settings` no longer matches `kiro`. The two lists differ
# in how far they are trusted:
#
#   distinct   -- believed anywhere, including whatever the pane prints
#   ambiguous  -- also an ordinary English word, so believed ONLY in the command
#                 that spawned the pane or the agent's own terminal title
#
# Use `replace = true` to drop luvus's built-in patterns instead of adding to
# them (the way to *remove* a default). Naming an agent luvus does not ship
# teaches it a new one, no rebuild needed.
#
# [identity]
# distinct = [\"cursor-agent\"]
# ambiguous = [\"cursor\"]
# replace = true

# ── state rules ──────────────────────────────────────────────────────────────
# One rule per [[rule]] block. `state` is working | blocked | idle.
# `region` is screen (the recent bottom text, default) or title (the OSC title).
# Conditions (all listed must hold): any / all / not (substring lists, case
# insensitive) and spinner (a running braille spinner glyph is visible).

[[rule]]
state = \"working\"
priority = 200
region = \"screen\"
any = [\"esc to interrupt\", \"esc to cancel\"]

[[rule]]
state = \"blocked\"
priority = 300
region = \"screen\"
all = [\"do you want to proceed\"]
not = [\"cancelled\"]
";

/// The JSON control-API socket path for this session (home-derived). Used by the
/// **server** to bind — never reads `$LUVUS_SOCKET_PATH`, or a server spawned
/// from inside a pane would try to bind its parent's socket.
pub fn socket_path() -> PathBuf {
    crate::session::api_socket_path_for(crate::session::active_name().as_deref())
}

/// The control socket a **CLI** should talk to: the one injected into this
/// process (a pane / module command carries `$LUVUS_SOCKET_PATH` pointing at its
/// own server), else the home-derived default. This is what makes `luvus …` run
/// inside a pane reach *that* session's server — so a module action that shells
/// out to `luvus`, or a `luvus module link` typed in a dev pane, targets the
/// instance you're in rather than whatever `$LUVUS_HOME` defaults to.
pub fn cli_socket_path() -> PathBuf {
    if crate::session::explicit_session_requested() {
        return socket_path();
    }
    match std::env::var_os("LUVUS_SOCKET_PATH") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => socket_path(),
    }
}

/// The binary client/render socket path for this session.
pub fn client_socket_path() -> PathBuf {
    crate::session::client_socket_path_for(crate::session::active_name().as_deref())
}

/// Build a snapshot from the live app.
/// Resolve every pane's native agent session for the snapshot without assigning
/// a conversation to the wrong pane.
///
/// A hook-reported id names its pane exactly, so it is always the source of
/// truth. Disk discovery can recover an unbound pane only when the mapping is
/// unambiguous: exactly one unbound pane and exactly one unclaimed session for
/// an `(agent, cwd)` pair.
///
/// Pane creation order is not agent-session creation order. In particular, a
/// user can make panes in several tabs and start their agents later in any order.
/// Matching the newest session to the newest pane therefore swaps conversations
/// between tabs on restart. When discovery cannot prove ownership, the pane
/// restores as a shell instead. That is recoverable and safe; resuming the wrong
/// conversation is neither.
fn resolve_pane_sessions(app: &App) -> HashMap<PaneId, Option<(String, String)>> {
    let mut out: HashMap<PaneId, Option<(String, String)>> = HashMap::new();
    let mut claimed: HashSet<(String, String)> = HashSet::new();
    let mut ids: Vec<PaneId> = app.status.keys().copied().collect();
    ids.sort_by_key(|p| p.0);

    // Pass 1: precise, hook-reported sessions take their id outright.
    for id in &ids {
        if let Some(a) = app.status.get(id).and_then(|s| s.agent_session.as_ref()) {
            let key = (a.agent.clone(), a.session_id.clone());
            if claimed.insert(key.clone()) {
                out.insert(*id, Some(key));
            } else {
                // A malformed or duplicate integration report must not make two
                // panes resume and write to the same native conversation.
                out.insert(*id, None);
            }
        }
    }

    // Pass 2: group unbound panes before looking at native session stores. A
    // `(agent, cwd)` identifies a set of possible conversations, not a pane.
    // Only a one-pane / one-session group can be recovered safely.
    let mut unbound: HashMap<(String, PathBuf), Vec<PaneId>> = HashMap::new();
    for id in ids {
        if out.contains_key(&id) {
            continue;
        }
        let Some(st) = app.status.get(&id) else {
            continue;
        };
        // The sidebar label is updated asynchronously. A restart can happen
        // before that update, while `proc_commands` already contains the live
        // process tree. In that short window `st.agent` is still the shell, so
        // looking up sessions with it would write no resume information and the
        // next server could only restore a bare shell. Use the process-derived
        // identity for this save when it is available; it is the same
        // authoritative source used by regular detection, but does not alter
        // the UI state or lifecycle bookkeeping.
        let agent = snapshot_agent(
            &app.manifests,
            app.proc_commands.get(&id).map(Vec::as_slice),
            &st.agent,
        );
        if let Some(pane) = app.panes.get(&id) {
            unbound
                .entry((agent, pane.cwd.clone()))
                .or_default()
                .push(id);
        }
    }
    for ((agent, cwd), pane_ids) in unbound {
        let sessions: Vec<String> = crate::agent::sessions_for(&agent, &cwd)
            .into_iter()
            .filter(|sid| !claimed.contains(&(agent.clone(), sid.clone())))
            .collect();
        if pane_ids.len() == 1 && sessions.len() == 1 {
            let id = pane_ids[0];
            let sid = sessions.into_iter().next().expect("length checked");
            claimed.insert((agent.clone(), sid.clone()));
            out.insert(id, Some((agent, sid)));
        } else {
            for id in pane_ids {
                out.insert(id, None);
            }
        }
    }
    out
}

/// The agent identity to use only while writing a restart snapshot.
///
/// `PaneStatus::agent` is intentionally asynchronous because it is a UI-facing
/// classification. `proc_commands` arrives first from the process scan, so it
/// is the safer identity during the small interval before the UI catches up.
/// If process information is unavailable, preserve the existing status-based
/// behaviour.
fn snapshot_agent(
    manifests: &crate::detect::Manifests,
    commands: Option<&[String]>,
    fallback: &str,
) -> String {
    commands
        .and_then(|commands| manifests.agent_in_processes(commands))
        .unwrap_or_else(|| fallback.to_string())
}

pub fn snapshot(app: &App) -> SessionSnapshot {
    let sessions = resolve_pane_sessions(app);
    let mut workspaces = Vec::new();
    for ws in &app.workspaces {
        let mut tabs = Vec::new();
        for tab in &ws.tabs {
            // A git tab (docs/17) has no real panes — record just the flag; it's
            // re-created as the dashboard (and re-fetched) on restore.
            if tab.is_git() {
                tabs.push(TabSnap {
                    id: tab.id.clone(),
                    tree: tab.layout.to_tree(),
                    focus: tab.layout.focus.0,
                    panes: Vec::new(),
                    git: true,
                    orch: false,
                    mission: false,
                    name: tab.name.clone(),
                });
                continue;
            }
            // An orchestration board (docs/22) has no real panes either.
            if tab.is_orch() {
                tabs.push(TabSnap {
                    id: tab.id.clone(),
                    tree: tab.layout.to_tree(),
                    focus: tab.layout.focus.0,
                    panes: Vec::new(),
                    git: false,
                    orch: true,
                    mission: false,
                    name: tab.name.clone(),
                });
                continue;
            }
            // A Mission Control dashboard (docs/54) — placeholder, re-derived.
            if tab.is_mission() {
                tabs.push(TabSnap {
                    id: tab.id.clone(),
                    tree: tab.layout.to_tree(),
                    focus: tab.layout.focus.0,
                    panes: Vec::new(),
                    git: false,
                    orch: false,
                    mission: true,
                    name: tab.name.clone(),
                });
                continue;
            }
            let panes = tab
                .layout
                .leaves()
                .into_iter()
                .filter_map(|id| {
                    // A file-view leaf (docs/38 FILE-3) is saved by its path and
                    // rebuilt on restore; it has no PTY.
                    if let Some(view) = app.views.get(&id) {
                        let (file, diff, preview) = match view {
                            crate::app::ViewKind::File(v) => (Some(v.path.clone()), None, None),
                            crate::app::ViewKind::Diff(v) => {
                                let status = app
                                    .diff
                                    .snapshot
                                    .as_ref()
                                    .and_then(|snapshot| {
                                        snapshot.files.iter().find(|file| file.key == v.key)
                                    })
                                    .map(|file| file.status)
                                    .unwrap_or(crate::diff::DiffFileStatus::Modified);
                                (
                                    None,
                                    Some(DiffSnap {
                                        root: v.root.clone(),
                                        key: v.key.clone(),
                                        status,
                                        preference: v.preference,
                                        scroll: v.scroll,
                                        selected: v.selected,
                                        selected_side: v.selected_side,
                                        horizontal: v.horizontal,
                                        wrap: v.wrap,
                                        context_lines: v.context_lines,
                                        show_line_numbers: v.show_line_numbers,
                                    }),
                                    None,
                                )
                            }
                            crate::app::ViewKind::Preview(v) => (
                                None,
                                None,
                                Some(PreviewSnap {
                                    path: v.path.clone(),
                                    kind: v.kind,
                                    scroll: v.scroll,
                                }),
                            ),
                        };
                        return Some((
                            id.0,
                            PaneSnap {
                                cwd: PathBuf::new(),
                                command: String::new(),
                                name: app.agent_name_for(id).map(|s| s.to_string()),
                                agent_session: None,
                                agent_launch: None,
                                screen: None,
                                module: None,
                                file,
                                diff,
                                preview,
                            },
                        ));
                    }
                    app.panes.get(&id).map(|p| {
                        // Resolved once for the whole snapshot so no two panes
                        // can claim the same session (see `resolve_pane_sessions`).
                        let agent_session = sessions.get(&id).cloned().flatten();
                        // The flags the agent was launched with, pulled from the
                        // live process argv the detection scan already captured
                        // (docs/62). Only for a recognized agent; a plain shell's
                        // argv never matches, so it stays None.
                        // Keyed off the agent in `agent_session` -- the one that
                        // will actually be resumed -- not the one detection
                        // currently sees. The two can disagree (a hook reports the
                        // session precisely, while detection reads the screen), and
                        // taking the detected name would hand one agent's options
                        // to another agent's resume command. `launch_args_for` then
                        // returns None when that agent has no command line in the
                        // pane, so a mismatch yields *no* options rather than the
                        // wrong ones.
                        let agent_launch = agent_session
                            .as_ref()
                            .map(|(k, _)| k.as_str())
                            .filter(|k| app.manifests.is_agent(k))
                            .and_then(|k| {
                                app.proc_commands
                                    .get(&id)
                                    .and_then(|cmds| app.manifests.launch_args_for(cmds, k))
                            })
                            .filter(|v| !v.is_empty());
                        // Capture the visible screen (cap size to keep saves light).
                        let screen = p
                            .engine
                            .lock()
                            .ok()
                            .map(|e| e.snapshot_ansi())
                            .filter(|s| s.len() < 256 * 1024);
                        let module = app
                            .module_panes
                            .get(&id)
                            .map(|r| (r.module_id.clone(), r.entrypoint.clone()));
                        (
                            id.0,
                            PaneSnap {
                                cwd: p.cwd.clone(),
                                command: p.command.clone(),
                                name: app.agent_name_for(id).map(|s| s.to_string()),
                                agent_session,
                                agent_launch,
                                screen,
                                module,
                                file: None,
                                diff: None,
                                preview: None,
                            },
                        )
                    })
                })
                .collect();
            tabs.push(TabSnap {
                id: tab.id.clone(),
                tree: tab.layout.to_tree(),
                focus: tab.layout.focus.0,
                panes,
                git: false,
                orch: false,
                mission: false,
                name: tab.name.clone(),
            });
        }
        workspaces.push(WsSnap {
            id: ws.id.clone(),
            name: ws.name.clone(),
            cwd: ws.cwd.clone(),
            active_tab: ws.active_tab,
            tabs,
            pinned: ws.pinned,
        });
    }
    SessionSnapshot {
        version: SNAPSHOT_VERSION,
        active_ws: app.active_ws,
        workspaces,
    }
}

/// Save the app's session atomically. A truly empty session can only remain after
/// restore or shell startup failure; clear its stale snapshot so the next start
/// cannot resurrect panes the user already closed.
pub fn save(app: &App) -> bool {
    let snap = snapshot(app);
    if snap.workspaces.is_empty() {
        if let Err(error) = fs::remove_file(session_path()) {
            if error.kind() != std::io::ErrorKind::NotFound {
                log_persist_failure("persist_clear");
                return false;
            }
        }
        crate::logging::event(
            crate::logging::EventKind::PersistCleared,
            &[crate::logging::Field::Reason(crate::logging::Reason::Empty)],
        );
        return true;
    }
    let dir = ensure_session_dir();
    if !dir.is_dir() {
        log_persist_failure("persist_dir");
        return false;
    }
    let Ok(json) = serde_json::to_string_pretty(&snap) else {
        log_persist_failure("persist_serialize");
        return false;
    };
    let path = session_path();
    let tmp = path.with_extension("json.tmp");
    let Ok(mut file) = fs::File::create(&tmp) else {
        log_persist_failure("persist_create");
        return false;
    };
    if file.write_all(json.as_bytes()).is_err() {
        log_persist_failure("persist_write");
        return false;
    }
    if file.flush().is_err() {
        log_persist_failure("persist_flush");
        return false;
    }
    if fs::rename(&tmp, &path).is_err() {
        log_persist_failure("persist_rename");
        return false;
    }
    crate::logging::event(
        crate::logging::EventKind::PersistSave,
        &[crate::logging::Field::Outcome(crate::logging::Outcome::Ok)],
    );
    true
}

fn log_persist_failure(error_code: &'static str) {
    crate::logging::event(
        crate::logging::EventKind::PersistSaveFailed,
        &[crate::logging::Field::ErrorCode(
            crate::logging::SafeId::new(error_code).expect("static id is valid"),
        )],
    );
}

/// Load a saved session, if one exists and parses at a known version.
pub fn load() -> Option<SessionSnapshot> {
    let data = fs::read_to_string(session_path()).ok()?;
    let snap: SessionSnapshot = serde_json::from_str(&data).ok()?;
    if snap.version > SNAPSHOT_VERSION {
        return None; // newer than we understand — ignore rather than misparse
    }
    Some(snap)
}

#[cfg(test)]
mod diff_snap_schema_tests {
    use super::*;

    #[test]
    fn diff_snapshot_display_state_defaults_without_dropping_the_session() {
        let path = crate::diff::RepoPath::from_path(std::path::Path::new("src/lib.rs")).unwrap();
        let snap = DiffSnap {
            root: PathBuf::from("repo"),
            key: crate::diff::DiffKey {
                repo_id: "repo".into(),
                worktree_id: "tree".into(),
                layer: crate::diff::DiffLayer::Worktree,
                old_path: Some(path.clone()),
                new_path: Some(path),
            },
            status: crate::diff::DiffFileStatus::Modified,
            preference: crate::diff::DiffLayoutPreference::Split,
            scroll: 4,
            selected: 5,
            selected_side: crate::diff::DiffSide::Old,
            horizontal: 6,
            wrap: true,
            context_lines: 7,
            show_line_numbers: false,
        };
        let mut value = serde_json::to_value(snap).unwrap();
        let object = value.as_object_mut().unwrap();
        for key in [
            "preference",
            "scroll",
            "selected",
            "selected_side",
            "horizontal",
            "wrap",
            "context_lines",
            "show_line_numbers",
        ] {
            object.remove(key);
        }

        let restored: DiffSnap = serde_json::from_value(value).unwrap();
        assert_eq!(restored.preference, crate::diff::DiffLayoutPreference::Auto);
        assert_eq!(restored.scroll, 0);
        assert_eq!(restored.selected, 0);
        assert_eq!(restored.selected_side, crate::diff::DiffSide::New);
        assert_eq!(restored.horizontal, 0);
        assert!(!restored.wrap);
        assert_eq!(restored.context_lines, 3);
        assert!(restored.show_line_numbers);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A CLI in a pane/module targets the injected socket (its own server), not
    /// the home-derived default — so `luvus …` inside a dev pane reaches the dev
    /// server, and a module action opening a pane hits the right instance.
    #[test]
    fn cli_socket_path_prefers_the_injected_socket() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = std::env::var_os("LUVUS_SOCKET_PATH");

        std::env::set_var("LUVUS_SOCKET_PATH", "/tmp/injected-luvus.sock");
        assert_eq!(cli_socket_path(), PathBuf::from("/tmp/injected-luvus.sock"));

        // Empty is treated as unset → falls back to the home-derived socket.
        std::env::set_var("LUVUS_SOCKET_PATH", "");
        assert_eq!(cli_socket_path(), socket_path());

        std::env::remove_var("LUVUS_SOCKET_PATH");
        assert_eq!(cli_socket_path(), socket_path());

        match saved {
            Some(v) => std::env::set_var("LUVUS_SOCKET_PATH", v),
            None => std::env::remove_var("LUVUS_SOCKET_PATH"),
        }
    }

    #[test]
    fn snapshot_uses_the_live_process_identity_before_sidebar_detection_catches_up() {
        let manifests = crate::detect::Manifests::builtin();
        let commands = vec![
            "/bin/zsh -l".to_string(),
            "codex --sandbox workspace-write".to_string(),
        ];

        assert_eq!(
            snapshot_agent(&manifests, Some(&commands), "zsh"),
            "codex",
            "a running resumable agent must not be snapshotted as its shell"
        );
        assert_eq!(
            snapshot_agent(&manifests, None, "zsh"),
            "zsh",
            "without a process scan, retain the existing safe fallback"
        );
    }

    // The control sockets grant command execution as the user, so the state
    // dir must be owner-only (0700) and each bound socket 0600 — regardless of
    // the process umask (see `ensure_config_dir` / `transport::bind`).
    #[test]
    fn closing_the_final_project_saves_the_home_terminal_replacement() {
        let _env = test_env("home-replacement-save");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        assert!(save(&app));
        assert!(session_path().exists(), "a live session snapshots");
        // Close the only pane. The project must not come back, but Luvus remains
        // usable through one neutral terminal rooted at home.
        let id = app.layout().focus;
        app.handle_event(crate::event::AppEvent::PtyExit(id));
        let home = crate::platform::home_dir().expect("test host has a home directory");
        assert_eq!(app.workspaces.len(), 1);
        assert!(crate::platform::same_path(&app.workspaces[0].cwd, &home));
        assert!(save(&app));
        let saved = load().expect("the replacement terminal is persisted");
        assert_eq!(saved.workspaces.len(), 1);
        assert!(crate::platform::same_path(&saved.workspaces[0].cwd, &home));
    }

    #[test]
    fn save_reports_failure_when_the_atomic_temp_file_cannot_be_created() {
        let _env = test_env("save-failure");
        let (tx, _rx) = std::sync::mpsc::channel();
        let app = App::new(80, 24, tx).unwrap();
        let tmp = session_path().with_extension("json.tmp");
        fs::create_dir_all(&tmp).unwrap();

        assert!(!save(&app), "the caller must retain its retry state");
        assert!(tmp.is_dir(), "the failure fixture remains in place");
    }

    #[test]
    fn state_dir_and_sockets_are_owner_only() {
        let _env = test_env("perms");
        let dir = ensure_config_dir();
        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "state dir is chmod 0700, got {mode:o}");

        let sock = dir.join("t.sock");
        let _listener = crate::ipc::transport::bind(&sock).unwrap();
        let mode = fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "socket is chmod 0600, got {mode:o}");
    }
}
