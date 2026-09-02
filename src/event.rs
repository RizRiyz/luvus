//! Messages flowing into the main loop from input/PTY threads and (in server
//! mode) from client connections.

use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;
use std::sync::Arc;

use ratatui::crossterm::event::{KeyEvent, MouseEvent};

use crate::ids::PaneId;
use crate::ipc::protocol::ServerMessage;
use crate::terminal::theme_probe::TerminalColors;

/// Input originating from one attached display client. Keeping the source id at
/// the server boundary lets the server select that client's geometry before it
/// performs hit-testing or forwards bytes to a pane.
pub enum ClientInput {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Paste(String),
    Resize(u16, u16),
}

pub enum AppEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Paste(String),
    Resize,
    /// The given pane produced output; the screen changed.
    PtyData(PaneId),
    /// The given pane's child process exited.
    PtyExit(PaneId),
    /// A deferred pane finished opening its PTY and now owns a root process and
    /// stable terminal-backend identity. Pending panes are deliberately absent
    /// from public inventory until this event is applied by the app loop.
    PtyReady {
        id: PaneId,
        cwd: std::path::PathBuf,
    },
    /// A terminal-backend create finished its filesystem and PTY work off-loop.
    /// The app loop performs only the bounded layout/index commit and response.
    BackendCreateReady {
        id: String,
        reply: Sender<String>,
        pane_id: PaneId,
        cwd: std::path::PathBuf,
        branch: Option<String>,
        worktree: Option<crate::git::WorktreeMembership>,
        commit: crate::terminal::backend::CreateCommit,
        result: Result<crate::terminal::pty::Pane, String>,
    },
    /// Resolve and validate an opt-in ANSI stream target on the single-writer
    /// app loop. Only cloneable read handles leave the loop; capture and socket
    /// writes happen on the requesting API worker.
    BackendObserve {
        params: serde_json::Value,
        reply: Sender<
            Result<crate::terminal::backend::ObserveTarget, crate::terminal::backend::BackendError>,
        >,
    },
    /// A binary client attached (server mode); `messages` feeds its socket writer.
    ClientConnected {
        id: u64,
        messages: Sender<ServerMessage>,
        /// At most one rendered frame may wait behind the socket writer. Control
        /// messages use the unbounded sender above, so clipboard writes and
        /// detach notifications can never be dropped merely because a frame is
        /// already queued.
        frame_pending: Arc<AtomicBool>,
        cols: u16,
        rows: u16,
        terminal_colors: Option<TerminalColors>,
    },
    /// A binary client detached.
    ClientDetach {
        id: u64,
    },
    /// Input from a binary display client. The server unwraps this only after
    /// activating the correct per-client viewport; it never reaches `App`.
    ClientInput {
        id: u64,
        input: ClientInput,
    },
    /// A module subprocess finished; fill in its log entry.
    ModuleCommandFinished {
        log_id: u64,
        code: Option<i32>,
        out: String,
        err: String,
    },
    /// The periodic resumable-session disk scan finished (run on a worker
    /// thread — the scan walks agent session stores and must never block the
    /// event loop).
    SessionsScanned(Vec<crate::agent::SessionInfo>),
    /// A FILES-dock directory read finished (docs/38): its sorted entries, run
    /// on a worker thread so the tree never blocks a frame on `read_dir`.
    DirRead {
        path: std::path::PathBuf,
        entries: Vec<crate::files::Entry>,
    },
    /// A bounded global-finder file-path catalog completed off the app loop.
    SearchFilesIndexed {
        instance: u64,
        catalogs: Vec<(
            usize,
            String,
            std::path::PathBuf,
            crate::search::files::FileCatalog,
        )>,
    },
    /// Fuzzy file/output scoring completed on the finder's dedicated worker.
    SearchResults {
        instance: u64,
        generation: u64,
        matches: Vec<crate::search::SearchMatch>,
        total: usize,
        capped: bool,
    },
    /// Bounded result rows returned by other running named-session owners.
    SearchFederatedResults {
        instance: u64,
        generation: u64,
        matches: Vec<crate::search::SearchMatch>,
        total: usize,
        partial: bool,
    },
    /// A target in another session was revalidated and focused by its owner.
    SearchHandoffReady {
        session: String,
        result: Result<(), String>,
    },
    /// A user-requested snapshot of known named sessions completed off-loop.
    NamedSessionsLoaded {
        generation: u64,
        result: Result<Vec<crate::session::SessionInfo>, String>,
    },
    /// The "Open Worktree" listing (`git worktree list` plus a stat per
    /// checkout) finished off-loop. Other checkouts may live on other mounts,
    /// so the scan must never run on the loop: a stale network path would
    /// freeze every attached client. `generation` discards a result whose
    /// modal was closed or reopened meanwhile.
    WorktreeListLoaded {
        generation: u64,
        result: Result<Vec<crate::git::model::Worktree>, String>,
    },
    /// A selected named session is ready for this client to attach.
    NamedSessionPrepared {
        generation: u64,
        name: String,
        result: Result<(), crate::app::session_menu::NamedSessionOpenError>,
    },
    /// One structured Git status scan feeds FILES tint and DIFF (docs/88).
    DiffStatus {
        token: u64,
        visible_root: std::path::PathBuf,
        result: Result<crate::diff::DiffSnapshot, String>,
    },
    /// One selected file's bounded diff finished loading off the app loop.
    DiffLoaded {
        id: PaneId,
        token: u64,
        result: Result<crate::diff::LoadedDiff, String>,
    },
    DiffNotesLoaded {
        review_id: String,
        result: Result<
            (
                Vec<crate::diff::ReviewNote>,
                crate::diff::notes::ReviewProgress,
            ),
            String,
        >,
    },
    DiffNoteSaved {
        note: crate::diff::ReviewNote,
        result: Result<(), String>,
    },
    DiffNoteRemoved {
        id: String,
        result: Result<(), String>,
    },
    DiffProgressSaved {
        result: Result<(), String>,
    },
    /// A file-view read finished (docs/38 FILE-3): applied to the view leaf `id`,
    /// but only if it is still the newest read that leaf asked for.
    ///
    /// A preview leaf is repointed at a new file without cancelling the read
    /// already in flight (`set_view_file`), and reads finish out of order.
    /// `token` is the `FileView::read_token` the read was issued with and is the
    /// invariant: newest wins. `path` is the file it carries — redundant while
    /// every scheduler bumps the token, kept as a cheap backstop so a future one
    /// that forgets cannot put another file's contents in this view.
    FileRead {
        id: PaneId,
        path: std::path::PathBuf,
        token: u64,
        load: crate::files::FileLoad,
    },
    /// Per-line git change markers for a file view finished computing
    /// (docs/38 + docs/30). Guarded exactly like `FileRead`, and it matters more
    /// here: markers ride the *same* worker after the text, so they are the
    /// later of the two to land.
    FileChanges {
        id: PaneId,
        path: std::path::PathBuf,
        token: u64,
        changes: Vec<crate::git::local::ChangeSpan>,
    },
    /// An explicit Markdown/Mermaid preview finished reading and parsing.
    /// Token + path + kind make stale results harmless when a view changes.
    PreviewRead {
        id: PaneId,
        path: std::path::PathBuf,
        kind: crate::files::preview::PreviewKind,
        token: u64,
        load: crate::files::preview::PreviewLoad,
    },
    /// A width-specific immutable preview layout finished off the app loop.
    PreviewLayout {
        id: PaneId,
        path: std::path::PathBuf,
        kind: crate::files::preview::PreviewKind,
        token: u64,
        key: crate::files::preview::LayoutKey,
        layout: std::sync::Arc<crate::files::preview::PreviewLayout>,
    },
    /// The periodic process scan finished: command lines running under each
    /// pane's child pid, from one `ps`. `None` means the platform cannot tell
    /// (Windows) or `ps` failed — detection then falls back to text heuristics
    /// rather than concluding that no agent is running.
    ProcScanned(Option<std::collections::HashMap<u32, Vec<String>>>),
    /// One process-table snapshot resolved every pane cwd, plus workspace
    /// branches and complete git-workspace candidates. Process and git probes
    /// run off-loop; the app loop only validates and mutates.
    CwdScanned {
        panes: Vec<(crate::ids::PaneId, crate::platform::PaneCwdEvidence)>,
        branches: Vec<(String, Option<String>)>,
        workspace_candidates: Vec<crate::git::GitRootInfo>,
    },
    /// A Mission Control usage scan finished (docs/54, MC-2/MC-4): best-effort
    /// tokens/context/cost keyed by agent + session id, read off-loop from native
    /// agent stores, plus each ledger's mtime so unchanged sessions stay cached.
    UsageScanned {
        scope: crate::mission::MissionScope,
        scanned: Vec<crate::mission::UsageKey>,
        usage: std::collections::HashMap<crate::mission::UsageKey, crate::mission::AgentUsage>,
        mtimes: std::collections::HashMap<crate::mission::UsageKey, std::time::SystemTime>,
    },
    /// A git-tab fetch finished; apply it to the matching `GitView`.
    GitData {
        view: u64,
        payload: crate::git::GitPayload,
    },
    /// A task's quality-gate command finished (ORCH-5): exit 0 → Done, else held
    /// at Review with the captured output.
    TaskGateFinished {
        task: String,
        code: Option<i32>,
        out: String,
    },
    /// A task branch finished integrating in the dedicated background
    /// worktree. Git runs off-loop; the app owner validates and commits the
    /// resulting task transition, event, and optional API reply.
    TaskMergeFinished {
        task: String,
        branch: String,
        previous: crate::orch::TaskStatus,
        integration_branch: String,
        result: Result<crate::git::local::MergeOutcome, String>,
        reply: Option<(String, Sender<String>)>,
    },
    /// The background update check found a newer release than this build (the
    /// version string, e.g. `"0.9.3"`). Shows the indicator by the version number.
    UpdateAvailable(String),
    /// An *asked-for* check finished (the changelog's "Check for updates"
    /// button). Carries the outcome so the answer can be shown either way.
    UpdateChecked(crate::update::CheckOutcome),
    /// A control-API request from a CLI invocation or module process. Arrives on
    /// the same channel as every other event so the loop wakes immediately —
    /// draining it on the idle tick would add a tick's latency to every CLI call.
    Api(crate::ipc::api::ApiRequest),
    /// A home-level theme scan completed on an API worker. The app loop only
    /// swaps the validated in-memory registry and applies the selected theme.
    ThemeReloaded {
        id: String,
        registry: crate::theme::ThemeRegistry,
        reply: Sender<String>,
    },
    /// Config file IO and parsing completed on the socket worker. The app loop
    /// only validates and swaps the resulting live configuration.
    ConfigReloaded {
        id: String,
        config: crate::config::Config,
        reply: Sender<String>,
    },
    /// Agent manifest IO and parsing completed on the socket worker.
    ManifestsReloaded {
        id: String,
        manifests: crate::detect::Manifests,
        reply: Sender<String>,
    },
    /// Settings requested removal of an installed theme. Filesystem validation,
    /// dependency checks, removal, and the follow-up scan all run on a worker;
    /// the app loop only swaps the completed registry and reports the outcome.
    ThemeUninstalled {
        id: String,
        result: Result<crate::theme::ThemeRegistry, String>,
    },
    /// A `wait.output` request (docs/81): reply once the pane's output contains
    /// the needle, or the deadline elapses. Carries its own reply channel so
    /// the connection can block without ever polling the loop.
    WaitOutput {
        id: String,
        pane: String,
        needle: String,
        timeout: Option<std::time::Duration>,
        reply: Sender<String>,
        cancelled: Arc<AtomicBool>,
    },
    /// Park an agent-state wait on the single-writer app loop. Registration and
    /// the initial comparison happen atomically relative to state transitions.
    AgentWait {
        id: String,
        pane: String,
        state: String,
        timeout: Option<std::time::Duration>,
        reply: Sender<String>,
        cancelled: Arc<AtomicBool>,
    },
    /// One server-owned agent launch. Pane selection/creation, command
    /// submission, readiness detection, and naming stay on the app loop so a
    /// client cannot interleave independent requests between those phases.
    AgentStart {
        id: String,
        params: serde_json::Value,
        reply: Sender<String>,
        cancelled: Arc<AtomicBool>,
    },
    /// Atomically queue one prompt and optionally park until the resulting turn
    /// reaches a requested semantic state. Output revision evidence covers fast
    /// turns whose Working state starts and finishes between detection ticks.
    AgentPrompt {
        id: String,
        params: serde_json::Value,
        reply: Sender<String>,
        cancelled: Arc<AtomicBool>,
    },
}
