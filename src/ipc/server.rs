//! Headless server (M2): owns the App + PTYs, renders into an off-screen
//! buffer, and streams frames to attached clients over the binary socket.
//! Input arrives from clients; the JSON API also runs here. See docs/03, docs/08.

use crate::ipc::transport::{self, Conn};
use std::collections::HashMap;
use std::io::{self, BufReader};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::app::App;
use crate::event::{AppEvent, ClientInput};
use crate::ipc::api;
use crate::ipc::protocol::{self, ClientMessage, ServerMessage};
use crate::persist;
use crate::ui;

const DEFAULT_SIZE: (u16, u16) = (120, 32);
/// Minimum time between rendered frames — the fps cap during activity (60fps).
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
/// A genuinely quiet server only needs a bounded maintenance/signalling audit.
/// This is still short enough for clean signal shutdown while cutting idle
/// timeout wakes by more than 80% compared with the former 33 ms poll.
const IDLE_INTERVAL: Duration = Duration::from_millis(250);
/// Detection hysteresis and parked API workflows retain their established
/// 100 ms cadence while they have time-sensitive work.
const FAST_IDLE_INTERVAL: Duration = Duration::from_millis(100);

fn frame_wait(elapsed_since_attempt: Duration) -> Duration {
    FRAME_INTERVAL
        .saturating_sub(elapsed_since_attempt)
        .max(Duration::from_millis(1))
}

fn frame_cadence_ready(elapsed_since_attempt: Duration) -> bool {
    elapsed_since_attempt >= FRAME_INTERVAL
}

static FRAMES_SENT: AtomicU64 = AtomicU64::new(0);
static FULL_FRAMES_SENT: AtomicU64 = AtomicU64::new(0);
static DIFF_RUNS_SENT: AtomicU64 = AtomicU64::new(0);
static FRAME_BYTES_SENT: AtomicU64 = AtomicU64::new(0);
static RENDER_PASSES: AtomicU64 = AtomicU64::new(0);
static CLIENT_PROJECTIONS: AtomicU64 = AtomicU64::new(0);
static CHANGED_PROJECTIONS: AtomicU64 = AtomicU64::new(0);
static UNCHANGED_PROJECTIONS: AtomicU64 = AtomicU64::new(0);
static FRAMES_ENQUEUED: AtomicU64 = AtomicU64::new(0);
static FRAMES_BACKPRESSURED: AtomicU64 = AtomicU64::new(0);
static FULL_TERMINAL_PROJECTIONS: AtomicU64 = AtomicU64::new(0);
static PARTIAL_TERMINAL_PROJECTIONS: AtomicU64 = AtomicU64::new(0);
static RETAINED_RENDER_FALLBACKS: AtomicU64 = AtomicU64::new(0);
static TERMINAL_DAMAGE_ROWS: AtomicU64 = AtomicU64::new(0);
static TERMINAL_DAMAGE_CELLS: AtomicU64 = AtomicU64::new(0);
static LOOP_EVENT_WAKES: AtomicU64 = AtomicU64::new(0);
static LOOP_DEADLINE_WAKES: AtomicU64 = AtomicU64::new(0);
static CAUSE_VISIBLE_PTY: AtomicU64 = AtomicU64::new(0);
static CAUSE_BACKGROUND_PTY: AtomicU64 = AtomicU64::new(0);
static CAUSE_DETECTION: AtomicU64 = AtomicU64::new(0);
static CAUSE_METADATA: AtomicU64 = AtomicU64::new(0);
// Retained in diagnostics for consumers that compare snapshots across releases.
// Working-state indicators are static, so this counter remains zero.
static CAUSE_ANIMATION: AtomicU64 = AtomicU64::new(0);
static CAUSE_UI: AtomicU64 = AtomicU64::new(0);
static CAUSE_API_MAINTENANCE: AtomicU64 = AtomicU64::new(0);
static CAUSE_FORCED: AtomicU64 = AtomicU64::new(0);
static CAUSE_RESYNC: AtomicU64 = AtomicU64::new(0);
static CAUSE_ATTACH_RESIZE: AtomicU64 = AtomicU64::new(0);

/// Process-lifetime frame counters for performance diagnostics.
pub fn performance_snapshot() -> serde_json::Value {
    serde_json::json!({
        "frames_sent": FRAMES_SENT.load(Ordering::Relaxed),
        "full_frames_sent": FULL_FRAMES_SENT.load(Ordering::Relaxed),
        "diff_runs_sent": DIFF_RUNS_SENT.load(Ordering::Relaxed),
        "frame_bytes_sent": FRAME_BYTES_SENT.load(Ordering::Relaxed),
        "render_passes": RENDER_PASSES.load(Ordering::Relaxed),
        "client_projections": CLIENT_PROJECTIONS.load(Ordering::Relaxed),
        "changed_projections": CHANGED_PROJECTIONS.load(Ordering::Relaxed),
        "unchanged_projections": UNCHANGED_PROJECTIONS.load(Ordering::Relaxed),
        "frames_enqueued": FRAMES_ENQUEUED.load(Ordering::Relaxed),
        "frames_backpressured": FRAMES_BACKPRESSURED.load(Ordering::Relaxed),
        "terminal_projection": {
            "full": FULL_TERMINAL_PROJECTIONS.load(Ordering::Relaxed),
            "partial": PARTIAL_TERMINAL_PROJECTIONS.load(Ordering::Relaxed),
            "fallbacks": RETAINED_RENDER_FALLBACKS.load(Ordering::Relaxed),
            "damage_rows": TERMINAL_DAMAGE_ROWS.load(Ordering::Relaxed),
            "damage_cells": TERMINAL_DAMAGE_CELLS.load(Ordering::Relaxed),
        },
        "render_causes": {
            "visible_pty": CAUSE_VISIBLE_PTY.load(Ordering::Relaxed),
            "background_pty": CAUSE_BACKGROUND_PTY.load(Ordering::Relaxed),
            "detection": CAUSE_DETECTION.load(Ordering::Relaxed),
            "metadata": CAUSE_METADATA.load(Ordering::Relaxed),
            "animation": CAUSE_ANIMATION.load(Ordering::Relaxed),
            "ui": CAUSE_UI.load(Ordering::Relaxed),
            "api_or_maintenance": CAUSE_API_MAINTENANCE.load(Ordering::Relaxed),
            "forced": CAUSE_FORCED.load(Ordering::Relaxed),
            "resync": CAUSE_RESYNC.load(Ordering::Relaxed),
            "client_attach_or_resize": CAUSE_ATTACH_RESIZE.load(Ordering::Relaxed),
            "unclassified": 0,
        },
        "loop_wakes": {
            "events": LOOP_EVENT_WAKES.load(Ordering::Relaxed),
            "deadlines": LOOP_DEADLINE_WAKES.load(Ordering::Relaxed),
        },
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RenderCause {
    VisiblePty,
    Detection,
    Metadata,
    UserInterface,
    ApiOrMaintenance,
    ForcedRepair,
    ClientResync,
    ClientAttachOrResize,
}

impl RenderCause {
    const fn bit(self) -> u16 {
        1 << self as u16
    }

    fn count(self) {
        let counter = match self {
            Self::VisiblePty => &CAUSE_VISIBLE_PTY,
            Self::Detection => &CAUSE_DETECTION,
            Self::Metadata => &CAUSE_METADATA,
            Self::UserInterface => &CAUSE_UI,
            Self::ApiOrMaintenance => &CAUSE_API_MAINTENANCE,
            Self::ForcedRepair => &CAUSE_FORCED,
            Self::ClientResync => &CAUSE_RESYNC,
            Self::ClientAttachOrResize => &CAUSE_ATTACH_RESIZE,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct RenderRequest {
    causes: u16,
    hidden_pty_activity: bool,
    visible_pty_activity: bool,
}

impl RenderRequest {
    fn record(&mut self, cause: RenderCause) {
        let bit = cause.bit();
        if self.causes & bit == 0 {
            self.causes |= bit;
            cause.count();
        }
    }

    fn record_visible_pty(&mut self) {
        self.visible_pty_activity = true;
        self.record(RenderCause::VisiblePty);
    }

    fn record_hidden_pty(&mut self) {
        self.hidden_pty_activity = true;
        CAUSE_BACKGROUND_PTY.fetch_add(1, Ordering::Relaxed);
    }

    fn needs_render(self) -> bool {
        self.causes != 0
    }

    fn is_visible_pty_only(self) -> bool {
        self.causes == RenderCause::VisiblePty.bit()
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

struct ClientSender {
    messages: Sender<ServerMessage>,
    frame_pending: Arc<AtomicBool>,
}

enum FrameSendError {
    Full,
    Disconnected,
}

impl ClientSender {
    /// Control messages are intentionally reliable. They are infrequent and
    /// small, while rendered frames retain their independent one-frame gate.
    fn send_control(&self, msg: ServerMessage) -> Result<(), ()> {
        self.messages.send(msg).map_err(|_| ())
    }

    /// Queue at most one frame while the socket writer is busy. Dropped frames
    /// are repaired by the existing `behind` full-frame resync path.
    fn try_send_frame(&self, msg: ServerMessage) -> Result<(), FrameSendError> {
        if self
            .frame_pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(FrameSendError::Full);
        }
        if self.messages.send(msg).is_err() {
            self.frame_pending.store(false, Ordering::Release);
            return Err(FrameSendError::Disconnected);
        }
        Ok(())
    }
}

struct ClientState {
    sender: ClientSender,
    size: (u16, u16),
    terminal_colors: Option<crate::terminal::theme_probe::TerminalColors>,
    render_buf: Buffer,
    last_frame: Option<protocol::FrameData>,
    behind: bool,
    force_full: bool,
    retained_pane_content: Vec<(crate::ids::PaneId, Rect)>,
    retained_ready: bool,
    last_activity: u64,
}

#[derive(Default)]
struct RenderScratch {
    damage: HashMap<crate::ids::PaneId, crate::terminal::vt::DamageSnapshot>,
    generations: HashMap<crate::ids::PaneId, u64>,
    order: Vec<u64>,
    dead: Vec<u64>,
}

impl ClientState {
    fn new(
        sender: ClientSender,
        cols: u16,
        rows: u16,
        terminal_colors: Option<crate::terminal::theme_probe::TerminalColors>,
        last_activity: u64,
    ) -> Self {
        let size = (cols.max(1), rows.max(1));
        Self {
            sender,
            size,
            terminal_colors,
            render_buf: Buffer::empty(Rect::new(0, 0, size.0, size.1)),
            last_frame: None,
            behind: false,
            force_full: true,
            retained_pane_content: Vec::new(),
            retained_ready: false,
            last_activity,
        }
    }

    fn send_control(&self, msg: ServerMessage) -> Result<(), ()> {
        self.sender.send_control(msg)
    }
}

type Clients = HashMap<u64, ClientState>;

pub fn run() -> Result<()> {
    let (tx, rx) = mpsc::channel::<AppEvent>();

    // Every process targeting one selected session serializes startup here. This is
    // deliberately before restoring panes: a losing server must exit without
    // spawning duplicate PTYs or retaining a second terminal grid.
    let state_dir = persist::ensure_server_session_dir()?;
    let startup_lock = transport::acquire_server_startup_lock(&state_dir)?;
    let sock = persist::socket_path();
    let client_sock = persist::client_socket_path();
    // A responsive listener means another server owns this state directory.
    // Do not reclaim either socket or start a competing process.
    if transport::endpoint_exists(&sock, Duration::from_millis(50))
        || transport::endpoint_exists(&client_sock, Duration::from_millis(50))
    {
        return Ok(());
    }
    let _logging = crate::logging::init(crate::logging::Role::Server);
    crate::logging::event(
        crate::logging::EventKind::ServerStart,
        &[crate::logging::Field::Role(crate::logging::Role::Server)],
    );
    api::set_socket_path(sock.clone());

    let events = api::new_bus();
    let api_listener = match api::bind_server(&sock, &startup_lock) {
        Ok(listener) => {
            crate::logging::event(
                crate::logging::EventKind::ListenerBind,
                &[crate::logging::Field::Listener(
                    crate::logging::Listener::Uhp,
                )],
            );
            listener
        }
        Err(error) => {
            crate::logging::event(
                crate::logging::EventKind::ListenerBindFailed,
                &[
                    crate::logging::Field::Listener(crate::logging::Listener::Uhp),
                    crate::logging::Field::ErrorCode(
                        crate::logging::SafeId::new("io").expect("static id is valid"),
                    ),
                ],
            );
            return Err(error.into());
        }
    };
    let client_listener = match bind_client_listener(&client_sock, &startup_lock) {
        Ok(listener) => {
            crate::logging::event(
                crate::logging::EventKind::ListenerBind,
                &[crate::logging::Field::Listener(
                    crate::logging::Listener::Client,
                )],
            );
            listener
        }
        Err(err) => {
            crate::logging::event(
                crate::logging::EventKind::ListenerBindFailed,
                &[
                    crate::logging::Field::Listener(crate::logging::Listener::Client),
                    crate::logging::Field::ErrorCode(
                        crate::logging::SafeId::new("io").expect("static id is valid"),
                    ),
                ],
            );
            drop(api_listener);
            let _ = remove_unbound_socket(&sock);
            return Err(err.into());
        }
    };

    let mut app = match App::restore_or_new(DEFAULT_SIZE.0, DEFAULT_SIZE.1, tx.clone()) {
        Ok(app) => app,
        Err(err) => {
            crate::logging::event(
                crate::logging::EventKind::PersistRestore,
                &[crate::logging::Field::Outcome(
                    crate::logging::Outcome::Error,
                )],
            );
            drop(client_listener);
            drop(api_listener);
            let _ = remove_unbound_socket(&client_sock);
            let _ = remove_unbound_socket(&sock);
            return Err(err);
        }
    };
    app.events = events.clone();
    app.server_mode = true;
    shutdown::install();

    let mut terminal_theme_enabled = app.config.theme == "terminal";
    let terminal_theme = Arc::new(AtomicBool::new(terminal_theme_enabled));
    api::start_server(api_listener, tx.clone(), events);
    start_client_listener(client_listener, tx.clone(), terminal_theme.clone());
    drop(startup_lock);
    let restored_workspaces = app.workspaces.len() as u64;
    let restored_tabs = app
        .workspaces
        .iter()
        .map(|workspace| workspace.tabs.len() as u64)
        .sum();
    let restored_panes = app.panes.len() as u64;
    crate::logging::event(
        crate::logging::EventKind::PersistRestore,
        &[
            crate::logging::Field::Outcome(crate::logging::Outcome::Ok),
            crate::logging::Field::RestoreWorkspaces(restored_workspaces),
            crate::logging::Field::RestoreTabs(restored_tabs),
            crate::logging::Field::RestorePanes(restored_panes),
            crate::logging::Field::RestoreSkipped(0),
        ],
    );
    crate::logging::event(
        crate::logging::EventKind::ServerReady,
        &[
            crate::logging::Field::RestoreWorkspaces(restored_workspaces),
            crate::logging::Field::RestoreTabs(restored_tabs),
            crate::logging::Field::RestorePanes(restored_panes),
            crate::logging::Field::RestoreSkipped(0),
        ],
    );
    let _pid_file = persist::ServerPidFile::claim();
    // The session is restored and the API socket is listening, so a module's
    // `[[startup]]` hooks can now call back in — this is where a module
    // repaints the docks it owns (docs/13 §3.7).
    app.run_module_startup_hooks();

    // Background "update available" check (off if the user disabled it).
    if app.config.check_updates {
        crate::update::spawn_check(tx.clone());
    }

    let mut clients: Clients = HashMap::new();
    let mut foreground: Option<u64> = None;
    let mut render_scratch = RenderScratch::default();
    // Geometry last committed to the shared PTYs and interactive hit-test state.
    // Secondary-client projections never change it.
    let mut interactive_size = DEFAULT_SIZE;
    let mut next_activity = 1u64;
    let mut last_render_attempt = Instant::now();
    let mut last_save = Instant::now();
    let mut immediate_save_attempted = false;
    // Un-rendered activity waiting for the frame cap to expire — drives a trailing
    // render so a change that lands mid-interval isn't stuck until the next event.
    let mut render_request = RenderRequest::default();
    // Fallback re-arm cadence for PTY wake coalescing when frames aren't being
    // rendered (no client attached / nothing dirty): readers may announce new
    // output ~10x/s. While rendering, the render path re-arms at the frame rate.
    let mut last_rearm = Instant::now();
    const REARM_INTERVAL: Duration = Duration::from_millis(100);

    loop {
        // Pending + clients attached → wait only until the cap frees up (flush
        // promptly); otherwise tick at the coarser idle cadence.
        let wait = if render_request.needs_render() && !clients.is_empty() {
            frame_wait(last_render_attempt.elapsed())
        } else {
            let mut idle = if app.needs_fast_runtime_tick(Instant::now()) {
                FAST_IDLE_INTERVAL
            } else {
                IDLE_INTERVAL
            };
            if app.has_pending_pty_output() {
                idle = idle.min(REARM_INTERVAL.saturating_sub(last_rearm.elapsed()));
            }
            idle.max(Duration::from_millis(1))
        };
        match rx.recv_timeout(wait) {
            Ok(ev) => {
                LOOP_EVENT_WAKES.fetch_add(1, Ordering::Relaxed);
                let source = event_render_source(&app, &ev);
                let changed = apply(
                    ev,
                    &mut app,
                    &mut clients,
                    &mut foreground,
                    &mut interactive_size,
                    &mut next_activity,
                );
                record_event_render_request(source, changed, &mut render_request);
            }
            Err(RecvTimeoutError::Timeout) => {
                LOOP_DEADLINE_WAKES.fetch_add(1, Ordering::Relaxed);
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
        while let Ok(ev) = rx.try_recv() {
            let source = event_render_source(&app, &ev);
            let changed = apply(
                ev,
                &mut app,
                &mut clients,
                &mut foreground,
                &mut interactive_size,
                &mut next_activity,
            );
            record_event_render_request(source, changed, &mut render_request);
        }
        let enabled = app.config.theme == "terminal";
        if enabled != terminal_theme_enabled {
            terminal_theme_enabled = enabled;
            terminal_theme.store(enabled, Ordering::Relaxed);
        }

        if app.should_quit {
            broadcast(
                &mut clients,
                ServerMessage::ServerShutdown {
                    reason: "server stopped".into(),
                },
            );
            break;
        }
        // A termination signal (kill, logout, system shutdown) requests a clean
        // exit: notify clients and fall through to the final session save below,
        // so the snapshot is current when the machine comes back.
        if shutdown::requested() {
            broadcast(
                &mut clients,
                ServerMessage::ServerShutdown {
                    reason: "server terminated".into(),
                },
            );
            break;
        }
        if !app.persist_session_now {
            immediate_save_attempted = false;
        }
        // Closing the final project bypasses the debounce once. Failed writes
        // retain both flags and retry at the normal cadence instead of hot-looping.
        let immediate_save_due = app.persist_session_now && !immediate_save_attempted;
        let debounced_save_due = app.session_dirty && last_save.elapsed() > Duration::from_secs(2);
        if immediate_save_due || debounced_save_due {
            immediate_save_attempted = app.persist_session_now;
            if persist::save(&app) {
                app.persist_session_now = false;
                app.session_dirty = false;
                immediate_save_attempted = false;
            }
            last_save = Instant::now();
        }
        if app.detach_requested {
            app.detach_requested = false;
            if let Some(id) = foreground.take() {
                if let Some(c) = clients.remove(&id) {
                    let _ = c.send_control(ServerMessage::Detach);
                }
                foreground = latest_client(&clients);
                apply_foreground_theme(&mut app, &clients, foreground);
                render_request.record(RenderCause::UserInterface);
            }
        }
        if let Some(name) = app.pending_session_switch.take() {
            if let Some(id) = foreground.take() {
                if let Some(client) = clients.remove(&id) {
                    let _ = client.send_control(ServerMessage::SwitchSession { name });
                }
                foreground = latest_client(&clients);
                apply_foreground_theme(&mut app, &clients, foreground);
                render_request.record(RenderCause::UserInterface);
            } else {
                app.show_toast("no attached client to switch".to_string());
            }
        }

        // A state transition here (e.g. a silent agent reaching Done) has no PtyData
        // to ride on, so repaint when detection reports a visible change.
        let now = Instant::now();
        if app.detect_tick(now) {
            render_request.record(RenderCause::Detection);
        }
        // Parked `wait.output` deadlines lapse on the tick (docs/81); a no-op
        // while nobody is waiting.
        app.tick_output_waits(now);
        app.tick_agent_waits(now);
        app.tick_agent_workflows(now);
        app.tick_backend_revision_waits(now);
        for msg in app.pending_notify.drain(..) {
            broadcast(&mut clients, ServerMessage::Notify(msg));
        }
        if let Some(signal) = app.pending_sound.take() {
            broadcast(&mut clients, ServerMessage::Sound(signal));
        }
        // A finished mouse selection copies to the client's clipboard (OSC 52).
        if let Some(url) = app.pending_open_url.take() {
            broadcast(&mut clients, ServerMessage::OpenUrl(url));
        }
        if let Some(text) = app.pending_clipboard.take() {
            broadcast(&mut clients, ServerMessage::Clipboard(text));
        }
        // An expired toast forces one render so it disappears (idle frames don't).
        if app.tick_toast(Instant::now()) {
            render_request.record(RenderCause::Metadata);
        }
        // Likewise for an expired search-jump flash (docs/63).
        if app.tick_search_flash(Instant::now()) {
            render_request.record(RenderCause::Metadata);
        }
        if app.tick_bar_notifications(now) {
            render_request.record(RenderCause::Metadata);
        }
        // Fallback re-arm (the render path below re-arms at the frame rate): a
        // flag still set here means un-rendered output → schedule a frame.
        if last_rearm.elapsed() >= REARM_INTERVAL {
            last_rearm = Instant::now();
            let (visible, background) = app.rearm_pty_notify_by_visibility();
            if visible {
                render_request.record_visible_pty();
            }
            if background {
                render_request.record_hidden_pty();
            }
        }

        // A forced redraw (resize / focus-regained / external damage) must render
        // even if nothing else changed this tick — and so must a client that is
        // waiting on its full-frame resync (see `needs_render`).
        let any_behind = clients.values().any(|client| client.behind);
        if app.force_redraw {
            render_request.record(RenderCause::ForcedRepair);
        }
        if any_behind {
            render_request.record(RenderCause::ClientResync);
        }

        if render_request.needs_render()
            && !clients.is_empty()
            && frame_cadence_ready(last_render_attempt.elapsed())
        {
            let forced = std::mem::take(&mut app.force_redraw);
            last_render_attempt = Instant::now();
            render_clients(
                &mut app,
                &mut clients,
                &mut foreground,
                &mut interactive_size,
                forced,
                render_request.is_visible_pty_only(),
                &mut render_scratch,
            );
            render_request.clear();
            // Re-arm the PTY readers now that their output is on screen. A flag
            // set during this frame = more output already waiting → stay dirty
            // so the burst keeps rendering at the frame cap, tail included.
            let (visible, background) = app.rearm_pty_notify_by_visibility();
            if visible {
                render_request.record_visible_pty();
            }
            if background {
                render_request.record_hidden_pty();
            }
        }
    }

    persist::save(&app);
    Ok(())
}

/// Apply a loop event; returns whether it warrants a redraw.
fn apply(
    ev: AppEvent,
    app: &mut App,
    clients: &mut Clients,
    foreground: &mut Option<u64>,
    interactive_size: &mut (u16, u16),
    next_activity: &mut u64,
) -> bool {
    match ev {
        AppEvent::ClientConnected {
            id,
            messages,
            frame_pending,
            cols,
            rows,
            terminal_colors,
        } => {
            crate::logging::event(
                crate::logging::EventKind::ServerClientAttach,
                &[
                    crate::logging::Field::ClientId(id),
                    crate::logging::Field::Cols(u64::from(cols)),
                    crate::logging::Field::Rows(u64::from(rows)),
                    crate::logging::Field::ProtocolVersion(u64::from(protocol::PROTOCOL_VERSION)),
                ],
            );
            let activity = *next_activity;
            *next_activity = next_activity.saturating_add(1);
            clients.insert(
                id,
                ClientState::new(
                    ClientSender {
                        messages,
                        frame_pending,
                    },
                    cols,
                    rows,
                    terminal_colors,
                    activity,
                ),
            );
            *foreground = Some(id);
            apply_foreground_theme(app, clients, *foreground);
            true
        }
        AppEvent::ClientDetach { id } => {
            crate::logging::event(
                crate::logging::EventKind::ServerClientDetach,
                &[
                    crate::logging::Field::ClientId(id),
                    crate::logging::Field::Reason(crate::logging::Reason::Eof),
                ],
            );
            let was_foreground = *foreground == Some(id);
            clients.remove(&id);
            if was_foreground {
                *foreground = latest_client(clients);
                apply_foreground_theme(app, clients, *foreground);
            }
            was_foreground
        }
        AppEvent::ClientInput { id, input } => {
            let Some(client) = clients.get_mut(&id) else {
                return false;
            };
            client.last_activity = *next_activity;
            *next_activity = next_activity.saturating_add(1);

            if let ClientInput::Resize(cols, rows) = input {
                crate::logging::event(
                    crate::logging::EventKind::ServerClientResize,
                    &[
                        crate::logging::Field::ClientId(id),
                        crate::logging::Field::Cols(u64::from(cols)),
                        crate::logging::Field::Rows(u64::from(rows)),
                    ],
                );
                client.size = (cols.max(1), rows.max(1));
                // Resize/focus repair is local to this terminal. Its next frame
                // must be complete, but other clients keep their diff baselines.
                client.force_full = true;
                return true;
            }

            // Input ownership follows actual interaction, not background resize
            // noise. Before hit-testing a newly active client, commit its view
            // geometry and PTY dimensions synchronously.
            let promoted = *foreground != Some(id);
            if promoted {
                *foreground = Some(id);
                apply_foreground_theme(app, clients, *foreground);
            }
            let target_size = clients.get(&id).map(|client| client.size);
            if promoted || target_size.is_some_and(|size| size != *interactive_size) {
                let no_damage = HashMap::new();
                let disconnected = clients.get_mut(&id).is_some_and(|client| {
                    render_client(app, client, true, false, false, &no_damage).disconnected
                });
                if disconnected {
                    clients.remove(&id);
                    *foreground = latest_client(clients);
                    apply_foreground_theme(app, clients, *foreground);
                    return true;
                }
                if let Some(size) = target_size {
                    *interactive_size = size;
                }
            }

            let event = match input {
                ClientInput::Key(key) => AppEvent::Key(key),
                ClientInput::Mouse(mouse) => AppEvent::Mouse(mouse),
                ClientInput::Paste(text) => AppEvent::Paste(text),
                ClientInput::Resize(..) => unreachable!("handled above"),
            };
            app.handle_event(event)
        }
        // Redraw only if the event actually changed the UI — a plain keystroke
        // forwarded to a pane does not (its echo arrives as a separate `PtyData`).
        other => app.handle_event(other),
    }
}

fn broadcast(clients: &mut Clients, msg: ServerMessage) {
    clients.retain(|_, client| client.send_control(msg.clone()).is_ok());
}

fn latest_client(clients: &Clients) -> Option<u64> {
    clients
        .iter()
        .max_by_key(|(_, client)| client.last_activity)
        .map(|(&id, _)| id)
}

fn apply_foreground_theme(app: &mut App, clients: &Clients, foreground: Option<u64>) {
    if app.config.theme != "terminal" {
        return;
    }
    if let Some(colors) = foreground
        .and_then(|id| clients.get(&id))
        .and_then(|client| client.terminal_colors.as_ref())
    {
        app.apply_terminal_colors(colors);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EventRenderSource {
    VisiblePty,
    HiddenPty,
    Cause(RenderCause),
}

fn event_render_source(app: &App, event: &AppEvent) -> EventRenderSource {
    match event {
        AppEvent::PtyData(id) if app.pane_is_visible(*id) => EventRenderSource::VisiblePty,
        AppEvent::PtyData(_) => EventRenderSource::HiddenPty,
        AppEvent::ClientConnected { .. }
        | AppEvent::ClientInput {
            input: ClientInput::Resize(..),
            ..
        } => EventRenderSource::Cause(RenderCause::ClientAttachOrResize),
        AppEvent::Api(_) => EventRenderSource::Cause(RenderCause::ApiOrMaintenance),
        _ => EventRenderSource::Cause(RenderCause::UserInterface),
    }
}

fn record_event_render_request(
    source: EventRenderSource,
    changed: bool,
    request: &mut RenderRequest,
) {
    if !changed {
        return;
    }
    match source {
        EventRenderSource::VisiblePty => request.record_visible_pty(),
        EventRenderSource::HiddenPty => request.record_hidden_pty(),
        EventRenderSource::Cause(cause) => request.record(cause),
    }
}

/// Whether this tick must render, even when nothing in the app changed.
///
/// `any_behind` is the subtle one. A client whose bounded channel was full
/// dropped that update and is marked `behind`; it is repaired by a **full
/// frame**, and [`send_frame`] only runs inside a render. So if the screen went
/// quiet at the moment a client fell behind — which is exactly what happens when
/// a burst of agent output ends — nothing would be dirty, no frame would render,
/// and that client would sit on a **stale** screen (missing whatever the dropped
/// diff carried) until some unrelated change happened to wake the loop. Treating
/// a pending resync as work to do closes that window to one frame interval.
/// Render the active client first so its geometry remains authoritative, then
/// render every other client as a projection at that client's own dimensions.
/// The common one-client case is still exactly one buffer reset, one UI render,
/// and one in-place diff.
fn render_clients(
    app: &mut App,
    clients: &mut Clients,
    foreground: &mut Option<u64>,
    interactive_size: &mut (u16, u16),
    force_all: bool,
    visible_pty_only: bool,
    scratch: &mut RenderScratch,
) -> bool {
    RENDER_PASSES.fetch_add(1, Ordering::Relaxed);
    if clients.is_empty() {
        return false;
    }
    if foreground.is_none_or(|id| !clients.contains_key(&id)) {
        *foreground = latest_client(clients);
        apply_foreground_theme(app, clients, *foreground);
    }

    let retained_client_ready = foreground
        .and_then(|id| clients.get(&id))
        .is_some_and(|client| {
            client.retained_ready
                && !client.force_full
                && !client.behind
                && client.last_frame.is_some()
        });
    let partial_candidate =
        visible_pty_only && !force_all && retained_client_ready && ui::retained_pty_eligible(app);
    if partial_candidate {
        capture_visible_terminal_damage(app, &mut scratch.damage);
    } else {
        capture_visible_terminal_generations(app, &mut scratch.generations);
    }
    let partial_pass = partial_candidate
        && !scratch.damage.is_empty()
        && scratch
            .damage
            .values()
            .all(|snapshot| snapshot.kind == crate::terminal::vt::DamageKind::Partial);

    scratch.order.clear();
    scratch.order.extend(clients.keys().copied());
    scratch
        .order
        .sort_unstable_by_key(|id| (*foreground != Some(*id), *id));
    scratch.dead.clear();
    let mut presented = false;
    for id in scratch.order.iter().copied() {
        let interactive = *foreground == Some(id);
        if let Some(client) = clients.get_mut(&id) {
            let outcome = render_client(
                app,
                client,
                interactive,
                force_all,
                partial_pass,
                &scratch.damage,
            );
            presented |= outcome.enqueued;
            if outcome.disconnected {
                scratch.dead.push(id);
            } else if interactive {
                *interactive_size = client.size;
            }
        }
    }
    for id in scratch.dead.drain(..) {
        clients.remove(&id);
    }
    if foreground.is_some_and(|id| !clients.contains_key(&id)) {
        *foreground = latest_client(clients);
        apply_foreground_theme(app, clients, *foreground);
    }
    if partial_candidate {
        acknowledge_visible_terminal_damage(app, &mut scratch.damage);
    } else {
        acknowledge_visible_terminal_generations(app, &mut scratch.generations);
    }
    presented
}

fn capture_visible_terminal_generations(
    app: &App,
    generations: &mut HashMap<crate::ids::PaneId, u64>,
) {
    generations.clear();
    if app.workspaces.is_empty()
        || app.active_is_git()
        || app.active_is_orch()
        || app.active_is_mission()
    {
        return;
    }
    let pane_ids = app.layout().leaves();
    generations.reserve(pane_ids.len());
    for id in pane_ids {
        let Some(pane) = app.panes.get(&id) else {
            continue;
        };
        if let Ok(engine) = pane.engine.lock() {
            generations.insert(id, engine.output_generation());
        }
    }
}

fn capture_visible_terminal_damage(
    app: &App,
    snapshots: &mut HashMap<crate::ids::PaneId, crate::terminal::vt::DamageSnapshot>,
) {
    snapshots.clear();
    if app.workspaces.is_empty()
        || app.active_is_git()
        || app.active_is_orch()
        || app.active_is_mission()
    {
        return;
    }
    let pane_ids = app.layout().leaves();
    snapshots.reserve(pane_ids.len());
    for id in pane_ids {
        let Some(pane) = app.panes.get(&id) else {
            continue;
        };
        if let Ok(mut engine) = pane.engine.lock() {
            let snapshot = engine.damage_snapshot();
            TERMINAL_DAMAGE_ROWS.fetch_add(snapshot.rows.len() as u64, Ordering::Relaxed);
            TERMINAL_DAMAGE_CELLS.fetch_add(
                snapshot
                    .rows
                    .iter()
                    .map(|row| row.cells.len() as u64)
                    .sum::<u64>(),
                Ordering::Relaxed,
            );
            snapshots.insert(id, snapshot);
        }
    }
}

fn acknowledge_visible_terminal_damage(
    app: &App,
    snapshots: &mut HashMap<crate::ids::PaneId, crate::terminal::vt::DamageSnapshot>,
) {
    for (id, snapshot) in snapshots.drain() {
        let Some(pane) = app.panes.get(&id) else {
            continue;
        };
        if let Ok(mut engine) = pane.engine.lock() {
            let _ = engine.acknowledge_damage(snapshot.generation);
            engine.recycle_damage_snapshot(snapshot);
        }
    }
}

fn acknowledge_visible_terminal_generations(
    app: &App,
    generations: &mut HashMap<crate::ids::PaneId, u64>,
) {
    for (id, generation) in generations.drain() {
        let Some(pane) = app.panes.get(&id) else {
            continue;
        };
        if let Ok(mut engine) = pane.engine.lock() {
            let _ = engine.acknowledge_damage(generation);
        }
    }
}

/// Render and enqueue one client's next frame. Returns true when its writer is
/// disconnected and the caller should remove it.
fn render_client(
    app: &mut App,
    client: &mut ClientState,
    interactive: bool,
    force_all: bool,
    partial_pass: bool,
    damage: &HashMap<crate::ids::PaneId, crate::terminal::vt::DamageSnapshot>,
) -> RenderClientOutcome {
    CLIENT_PROJECTIONS.fetch_add(1, Ordering::Relaxed);
    let area = Rect::new(0, 0, client.size.0, client.size.1);
    if client.render_buf.area != area {
        client.render_buf = Buffer::empty(area);
        client.last_frame = None;
        client.force_full = true;
        client.retained_ready = false;
    }

    let may_patch = partial_pass
        && interactive
        && client.retained_ready
        && !client.force_full
        && !client.behind
        && client.last_frame.is_some();
    let patched = if may_patch {
        let mut target = ui::RenderTarget::new(&mut client.render_buf, area);
        ui::patch_terminal_damage(&mut target, app, &client.retained_pane_content, damage)
            .map(|()| (target.cursor(), target.cursor_visible()))
            .ok()
    } else {
        None
    };

    let (cursor, cursor_visible) = if let Some(cursor) = patched {
        PARTIAL_TERMINAL_PROJECTIONS.fetch_add(1, Ordering::Relaxed);
        cursor
    } else {
        if partial_pass {
            RETAINED_RENDER_FALLBACKS.fetch_add(1, Ordering::Relaxed);
        }
        FULL_TERMINAL_PROJECTIONS.fetch_add(1, Ordering::Relaxed);
        client.render_buf.reset();
        let mut target = ui::RenderTarget::new(&mut client.render_buf, area);
        if interactive {
            ui::render_into(&mut target, app);
            client
                .retained_pane_content
                .clone_from(&app.pane_content_rects);
            client.retained_ready = true;
        } else {
            ui::render_projection(&mut target, app);
            client.retained_ready = false;
        }
        (target.cursor(), target.cursor_visible())
    };

    let full = force_all
        || client.force_full
        || client.behind
        || client.last_frame.as_ref().is_none_or(|previous| {
            previous.width != client.render_buf.area.width
                || previous.height != client.render_buf.area.height
        });
    let message = if full {
        client.last_frame = Some(protocol::frame_from_buffer(
            &client.render_buf,
            cursor,
            cursor_visible,
        ));
        Some(ServerMessage::Frame(
            client.last_frame.as_ref().expect("frame stored").clone(),
        ))
    } else {
        let previous = client.last_frame.as_mut().expect("frame baseline exists");
        let cursor_moved = previous.cursor != cursor || previous.cursor_visible != cursor_visible;
        let runs = protocol::diff_buffer(previous, &client.render_buf);
        previous.cursor = cursor;
        previous.cursor_visible = cursor_visible;
        if runs.is_empty() && !cursor_moved {
            None
        } else {
            Some(ServerMessage::FrameDiff(protocol::FrameDiff {
                width: previous.width,
                height: previous.height,
                runs,
                cursor,
                cursor_visible,
            }))
        }
    };

    let Some(message) = message else {
        UNCHANGED_PROJECTIONS.fetch_add(1, Ordering::Relaxed);
        return RenderClientOutcome::default();
    };
    CHANGED_PROJECTIONS.fetch_add(1, Ordering::Relaxed);
    match client.sender.try_send_frame(message) {
        Ok(()) => {
            FRAMES_ENQUEUED.fetch_add(1, Ordering::Relaxed);
            client.behind = false;
            client.force_full = false;
            RenderClientOutcome {
                enqueued: true,
                disconnected: false,
            }
        }
        Err(FrameSendError::Full) => {
            FRAMES_BACKPRESSURED.fetch_add(1, Ordering::Relaxed);
            client.behind = true;
            client.retained_ready = false;
            RenderClientOutcome::default()
        }
        Err(FrameSendError::Disconnected) => RenderClientOutcome {
            enqueued: false,
            disconnected: true,
        },
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct RenderClientOutcome {
    enqueued: bool,
    disconnected: bool,
}

fn bind_client_listener(
    path: &Path,
    startup_lock: &transport::ServerStartupLock,
) -> io::Result<transport::Listener> {
    startup_lock.reclaim_stale_socket(path)?;
    transport::bind(path)
}

fn start_client_listener(
    listener: transport::Listener,
    app_tx: Sender<AppEvent>,
    terminal_theme: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        for (id, stream) in (1u64..).zip(transport::incoming(&listener)) {
            let app_tx = app_tx.clone();
            let terminal_theme = terminal_theme.clone();
            thread::spawn(move || handle_client(id, stream, app_tx, terminal_theme));
        }
    });
}

/// Remove a listener pathname only after its listener has been dropped and the
/// startup lock is still held. Named pipes have no filesystem path to clean up.
fn remove_unbound_socket(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        let _ = path;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    }
}

fn handle_client(id: u64, stream: Conn, app_tx: Sender<AppEvent>, terminal_theme: Arc<AtomicBool>) {
    let mut reader = BufReader::new(stream.clone());
    let mut writer = stream;

    let (cols, rows) = match protocol::read_message::<_, ClientMessage>(&mut reader) {
        Ok(ClientMessage::Hello {
            version,
            cols,
            rows,
        }) => {
            if version != protocol::PROTOCOL_VERSION {
                crate::logging::event(
                    crate::logging::EventKind::ServerClientHandshakeRejected,
                    &[
                        crate::logging::Field::Reason(crate::logging::Reason::VersionMismatch),
                        crate::logging::Field::ProtocolVersion(u64::from(version)),
                    ],
                );
                let _ = protocol::write_message(
                    &mut writer,
                    &ServerMessage::Welcome {
                        version: protocol::PROTOCOL_VERSION,
                        error: Some("protocol version mismatch".into()),
                    },
                );
                return;
            }
            (cols, rows)
        }
        _ => return,
    };

    if protocol::write_message(
        &mut writer,
        &ServerMessage::Welcome {
            version: protocol::PROTOCOL_VERSION,
            error: None,
        },
    )
    .is_err()
    {
        return;
    }

    let probe_terminal = terminal_theme.load(Ordering::Relaxed);
    if protocol::write_message(&mut writer, &ServerMessage::Ready { probe_terminal }).is_err() {
        return;
    }
    let terminal_colors = if probe_terminal {
        match protocol::read_message::<_, ClientMessage>(&mut reader) {
            Ok(ClientMessage::TerminalColors(colors)) => colors,
            _ => return,
        }
    } else {
        None
    };

    let (message_tx, message_rx) = mpsc::channel::<ServerMessage>();
    let frame_pending = Arc::new(AtomicBool::new(false));
    let writer_frame_pending = frame_pending.clone();
    thread::spawn(move || {
        for msg in message_rx {
            let frame_stats = match &msg {
                ServerMessage::Frame(_) => Some((true, 0usize)),
                ServerMessage::FrameDiff(frame) => Some((false, frame.runs.len())),
                _ => None,
            };
            if frame_stats.is_some() {
                // Match sync_channel(1): receiving frees the single frame slot,
                // even while the socket write itself is still in progress.
                writer_frame_pending.store(false, Ordering::Release);
            }
            let stop = matches!(
                msg,
                ServerMessage::Detach
                    | ServerMessage::ServerShutdown { .. }
                    | ServerMessage::SwitchSession { .. }
            );
            match protocol::write_message_counted(&mut writer, &msg) {
                Ok(bytes) => {
                    if let Some((full, runs)) = frame_stats {
                        FRAMES_SENT.fetch_add(1, Ordering::Relaxed);
                        FULL_FRAMES_SENT.fetch_add(u64::from(full), Ordering::Relaxed);
                        DIFF_RUNS_SENT.fetch_add(runs as u64, Ordering::Relaxed);
                        FRAME_BYTES_SENT.fetch_add(bytes as u64, Ordering::Relaxed);
                    }
                    if stop {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    if app_tx
        .send(AppEvent::ClientConnected {
            id,
            messages: message_tx,
            frame_pending,
            cols,
            rows,
            terminal_colors,
        })
        .is_err()
    {
        return;
    }

    loop {
        match protocol::read_message::<_, ClientMessage>(&mut reader) {
            Ok(ClientMessage::Key(k)) => {
                if app_tx
                    .send(AppEvent::ClientInput {
                        id,
                        input: ClientInput::Key(k),
                    })
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMessage::Mouse(m)) => {
                if app_tx
                    .send(AppEvent::ClientInput {
                        id,
                        input: ClientInput::Mouse(m),
                    })
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMessage::Paste(s)) => {
                if app_tx
                    .send(AppEvent::ClientInput {
                        id,
                        input: ClientInput::Paste(s),
                    })
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMessage::Resize { cols, rows }) => {
                if app_tx
                    .send(AppEvent::ClientInput {
                        id,
                        input: ClientInput::Resize(cols, rows),
                    })
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMessage::Detach) | Err(_) => {
                let _ = app_tx.send(AppEvent::ClientDetach { id });
                break;
            }
            Ok(ClientMessage::Hello { .. } | ClientMessage::TerminalColors(_)) => {}
        }
    }
}

/// Graceful shutdown on a termination signal. The handler only flips an atomic
/// flag (the only async-signal-safe thing to do); the event loop polls it every
/// idle tick (≤250ms) and exits through the normal path — clients notified, the
/// session saved — instead of dying mid-state on SIGTERM (logout, `kill`,
/// system shutdown).
#[cfg(unix)]
mod shutdown {
    use std::sync::atomic::{AtomicBool, Ordering};

    static FLAG: AtomicBool = AtomicBool::new(false);

    pub fn requested() -> bool {
        FLAG.load(Ordering::Relaxed)
    }

    pub fn install() {
        extern "C" fn on_signal(_sig: libc::c_int) {
            FLAG.store(true, Ordering::Relaxed);
        }
        unsafe {
            let h = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
            libc::signal(libc::SIGTERM, h);
            libc::signal(libc::SIGHUP, h);
            libc::signal(libc::SIGINT, h);
        }
    }
}

/// Windows: no POSIX signals; the detached server is stopped via `server stop`.
#[cfg(not(unix))]
mod shutdown {
    pub fn requested() -> bool {
        false
    }

    pub fn install() {}
}

#[cfg(test)]
mod tests {
    use super::ServerMessage;
    use super::{
        apply, broadcast, frame_cadence_ready, frame_wait, record_event_render_request,
        render_clients, ClientSender, ClientState, EventRenderSource, FrameSendError, RenderCause,
        RenderRequest, RenderScratch, FRAME_INTERVAL,
    };
    use crate::app::App;
    use crate::event::{AppEvent, ClientInput};
    use crate::ipc::protocol::FrameDiff;
    use crate::terminal::appearance::PaneAppearance;
    use crate::terminal::vt::{create_engine, VtEngineKind};
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::Duration;

    fn display_client(
        cols: u16,
        rows: u16,
        activity: u64,
    ) -> (ClientState, mpsc::Receiver<ServerMessage>) {
        let (messages, rx) = mpsc::channel();
        (
            ClientState::new(
                ClientSender {
                    messages,
                    frame_pending: Arc::new(AtomicBool::new(false)),
                },
                cols,
                rows,
                None,
                activity,
            ),
            rx,
        )
    }

    fn received_frame_size(rx: &mpsc::Receiver<ServerMessage>) -> (u16, u16) {
        match rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            ServerMessage::Frame(frame) => (frame.width, frame.height),
            ServerMessage::FrameDiff(frame) => (frame.width, frame.height),
            _ => panic!("expected rendered frame"),
        }
    }

    #[test]
    fn visible_pty_only_frame_uses_retained_rows_after_full_baseline() {
        let _env = crate::persist::test_env("server-retained-terminal-rows");
        let (app_tx, _app_rx) = mpsc::channel();
        let mut app = App::new(100, 30, app_tx).expect("app starts");
        app.server_mode = true;
        let focus = app.layout().focus;
        let (response_tx, _response_rx) = mpsc::channel();
        let engine = create_engine(
            VtEngineKind::Alacritty,
            100,
            30,
            response_tx,
            4 * 1024 * 1024,
            PaneAppearance::default(),
        );
        app.panes.get_mut(&focus).expect("focused pane").engine = engine.clone();

        let (client, rx) = display_client(100, 30, 1);
        let mut clients = HashMap::from([(1, client)]);
        let mut foreground = Some(1);
        let mut interactive_size = (100, 30);
        let mut scratch = RenderScratch::default();
        assert!(render_clients(
            &mut app,
            &mut clients,
            &mut foreground,
            &mut interactive_size,
            false,
            false,
            &mut scratch,
        ));
        assert!(matches!(rx.recv().unwrap(), ServerMessage::Frame(_)));
        clients[&1]
            .sender
            .frame_pending
            .store(false, Ordering::Release);

        engine
            .lock()
            .expect("engine lock")
            .advance(b"one changed row");
        let before = super::PARTIAL_TERMINAL_PROJECTIONS.load(Ordering::Relaxed);
        assert!(render_clients(
            &mut app,
            &mut clients,
            &mut foreground,
            &mut interactive_size,
            false,
            true,
            &mut scratch,
        ));
        assert!(matches!(rx.recv().unwrap(), ServerMessage::FrameDiff(_)));
        assert!(
            super::PARTIAL_TERMINAL_PROJECTIONS.load(Ordering::Relaxed) > before,
            "PTY-only render patched the retained client buffer"
        );
        assert!(!clients[&1].behind);

        // A slow client may reject the next partial frame. Its retained buffer
        // is then deliberately abandoned and the next accepted render is a
        // complete resynchronization, so acknowledging shared VT damage cannot
        // leave that client permanently behind.
        engine.lock().expect("engine lock").advance(b" more");
        assert!(!render_clients(
            &mut app,
            &mut clients,
            &mut foreground,
            &mut interactive_size,
            false,
            true,
            &mut scratch,
        ));
        assert!(clients[&1].behind);
        assert!(!clients[&1].retained_ready);
        clients[&1]
            .sender
            .frame_pending
            .store(false, Ordering::Release);
        assert!(render_clients(
            &mut app,
            &mut clients,
            &mut foreground,
            &mut interactive_size,
            false,
            false,
            &mut scratch,
        ));
        assert!(matches!(rx.recv().unwrap(), ServerMessage::Frame(_)));
        assert!(!clients[&1].behind);
    }

    #[test]
    fn hidden_pty_activity_does_not_request_presentation() {
        let mut request = RenderRequest::default();
        record_event_render_request(EventRenderSource::HiddenPty, true, &mut request);
        assert!(request.hidden_pty_activity);
        assert!(!request.needs_render());

        record_event_render_request(EventRenderSource::VisiblePty, true, &mut request);
        assert!(request.visible_pty_activity);
        assert!(request.needs_render());
    }

    #[test]
    fn forced_and_resync_causes_request_repair_frames() {
        let mut request = RenderRequest::default();
        request.record(RenderCause::ForcedRepair);
        request.record(RenderCause::ClientResync);
        assert!(request.needs_render());
        request.clear();
        assert!(!request.needs_render());
    }

    #[test]
    fn unchanged_normal_render_attempts_remain_frame_capped() {
        let mut request = RenderRequest::default();
        request.record(RenderCause::VisiblePty);

        // An unchanged projection clears the request but still resets the
        // attempt clock. A new normal request must wait for the same frame cap.
        for _ in 0..2 {
            assert!(request.needs_render());
            assert_eq!(frame_wait(Duration::ZERO), FRAME_INTERVAL);
            assert!(!frame_cadence_ready(Duration::ZERO));
            request.clear();
            request.record(RenderCause::VisiblePty);
        }

        assert!(!frame_cadence_ready(
            FRAME_INTERVAL - Duration::from_millis(1)
        ));
        assert!(frame_cadence_ready(FRAME_INTERVAL));
    }

    /// A tab switch requests a frame at the same time a finished selection sends
    /// its clipboard payload. Frames may be dropped and repaired, but clipboard
    /// writes must remain queued or the next paste uses stale clipboard content.
    #[test]
    fn clipboard_is_reliable_when_a_tab_frame_is_already_queued() {
        let (messages, rx) = mpsc::channel();
        let client = ClientState::new(
            ClientSender {
                messages,
                frame_pending: Arc::new(AtomicBool::new(false)),
            },
            120,
            32,
            None,
            1,
        );
        let frame = || {
            ServerMessage::FrameDiff(FrameDiff {
                width: 120,
                height: 32,
                runs: Vec::new(),
                cursor: None,
                cursor_visible: false,
            })
        };

        assert!(client.sender.try_send_frame(frame()).is_ok());
        // Frame backpressure is still one deep, so output bursts cannot build an
        // unbounded queue while a client is slow.
        assert!(
            matches!(
                client.sender.try_send_frame(frame()),
                Err(FrameSendError::Full)
            ),
            "a second frame remains coalesced into the resync path"
        );

        let mut clients = HashMap::from([(7, client)]);
        broadcast(
            &mut clients,
            ServerMessage::Clipboard("exact selection".into()),
        );

        assert!(
            matches!(rx.recv().unwrap(), ServerMessage::FrameDiff(_)),
            "the already queued tab frame stays first"
        );
        assert!(
            matches!(
                rx.recv().unwrap(),
                ServerMessage::Clipboard(text) if text == "exact selection"
            ),
            "clipboard control data cannot be dropped behind a frame"
        );
    }

    /// Explicit client detach is carried by the same reliable control path and
    /// cannot be lost behind a queued frame.
    #[test]
    fn explicit_detach_is_delivered_behind_a_queued_frame() {
        let (messages, rx) = mpsc::channel();
        let client = ClientSender {
            messages,
            frame_pending: Arc::new(AtomicBool::new(false)),
        };
        assert!(client
            .try_send_frame(ServerMessage::FrameDiff(FrameDiff {
                width: 1,
                height: 1,
                runs: Vec::new(),
                cursor: None,
                cursor_visible: false,
            }))
            .is_ok());
        assert!(client.send_control(ServerMessage::Detach).is_ok());

        assert!(matches!(rx.recv().unwrap(), ServerMessage::FrameDiff(_)));
        assert!(matches!(rx.recv().unwrap(), ServerMessage::Detach));
    }

    #[test]
    fn different_client_sizes_receive_independent_frames_and_active_geometry() {
        let _env = crate::persist::test_env("multi-client-resolution");
        let (app_tx, _app_rx) = mpsc::channel();
        let mut app = App::new(120, 40, app_tx).expect("app starts");
        app.server_mode = true;

        let (large, large_rx) = display_client(120, 40, 2);
        let (small, small_rx) = display_client(40, 18, 1);
        let mut clients = HashMap::from([(1, large), (2, small)]);
        let mut foreground = Some(1);
        let mut interactive_size = (120, 40);
        let mut scratch = RenderScratch::default();

        render_clients(
            &mut app,
            &mut clients,
            &mut foreground,
            &mut interactive_size,
            false,
            false,
            &mut scratch,
        );

        assert_eq!(received_frame_size(&large_rx), (120, 40));
        assert_eq!(received_frame_size(&small_rx), (40, 18));
        assert_eq!(clients[&1].last_frame.as_ref().unwrap().width, 120);
        assert_eq!(clients[&2].last_frame.as_ref().unwrap().width, 40);
        assert_eq!(interactive_size, (120, 40));
        assert!(!app.compact, "secondary compact projection must not leak");

        let focus = app.layout().focus;
        let content = app
            .pane_content_rects
            .iter()
            .find_map(|(id, rect)| (*id == focus).then_some(*rect))
            .expect("active pane content");
        assert_eq!(
            app.panes[&focus].size(),
            (content.width, content.height),
            "secondary projection must not resize the shared PTY"
        );
    }

    #[test]
    fn background_resize_is_local_and_interaction_promotes_its_view() {
        let _env = crate::persist::test_env("multi-client-promotion");
        let (app_tx, _app_rx) = mpsc::channel();
        let mut app = App::new(120, 40, app_tx).expect("app starts");
        app.server_mode = true;
        let (large, _large_rx) = display_client(120, 40, 2);
        let (small, small_rx) = display_client(50, 20, 1);
        let mut clients = HashMap::from([(1, large), (2, small)]);
        let mut foreground = Some(1);
        let mut interactive_size = (120, 40);
        let mut next_activity = 3;

        assert!(apply(
            AppEvent::ClientInput {
                id: 2,
                input: ClientInput::Resize(46, 16),
            },
            &mut app,
            &mut clients,
            &mut foreground,
            &mut interactive_size,
            &mut next_activity,
        ));
        assert_eq!(foreground, Some(1), "background resize cannot steal input");
        assert_eq!(clients[&2].size, (46, 16));
        assert_eq!(interactive_size, (120, 40));

        assert!(!apply(
            AppEvent::ClientInput {
                id: 2,
                input: ClientInput::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE,)),
            },
            &mut app,
            &mut clients,
            &mut foreground,
            &mut interactive_size,
            &mut next_activity,
        ));
        assert_eq!(foreground, Some(2));
        assert_eq!(interactive_size, (46, 16));
        assert!(app.compact, "the newly active narrow client owns its view");
        assert_eq!(received_frame_size(&small_rx), (46, 16));
    }
}
