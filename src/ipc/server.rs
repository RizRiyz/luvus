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
use crate::ipc::protocol::{self, ClientMessage, ServerMessage, SurfaceInterest};
use crate::persist;
use crate::ui;

const DEFAULT_SIZE: (u16, u16) = (120, 32);
/// Minimum time between rendered frames — the fps cap during activity (60fps).
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const SESSION_SAVE_DEBOUNCE: Duration = Duration::from_secs(2);

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
    cell_width_px: u16,
    cell_height_px: u16,
    terminal_colors: Option<crate::terminal::theme_probe::TerminalColors>,
    render_buf: Buffer,
    last_frame: Option<protocol::FrameData>,
    behind: bool,
    force_full: bool,
    retained_pane_content: Vec<(crate::ids::PaneId, Rect)>,
    retained_ready: bool,
    last_activity: u64,
    interest: SurfaceInterest,
    prepare_ticket: Option<u64>,
    shell_dock_layout: protocol::ShellDockLayout,
    shell_sidebars: Option<protocol::ShellSidebars>,
    sidebar_cache: Option<crate::app::Sidebars>,
    last_shell_sidebars: Option<protocol::ShellSidebars>,
    machine_capable: bool,
    last_shell_dock: Option<protocol::ShellDockRect>,
    last_shell_workspaces: Vec<protocol::ShellWorkspace>,
    shell_dock_dirty: bool,
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
        cell_width_px: u16,
        cell_height_px: u16,
        terminal_colors: Option<crate::terminal::theme_probe::TerminalColors>,
        last_activity: u64,
    ) -> Self {
        let size = (cols.max(1), rows.max(1));
        Self {
            sender,
            size,
            cell_width_px,
            cell_height_px,
            terminal_colors,
            render_buf: Buffer::empty(Rect::new(0, 0, size.0, size.1)),
            last_frame: None,
            behind: false,
            force_full: true,
            retained_pane_content: Vec::new(),
            retained_ready: false,
            last_activity,
            interest: SurfaceInterest::Active,
            prepare_ticket: None,
            shell_dock_layout: protocol::ShellDockLayout::default(),
            shell_sidebars: None,
            sidebar_cache: None,
            last_shell_sidebars: None,
            machine_capable: false,
            last_shell_dock: None,
            last_shell_workspaces: Vec::new(),
            shell_dock_dirty: false,
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
    app.reconcile_automations();
    shutdown::install(tx.clone());

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
        // Pending + clients attached → wait only until the cap frees up.
        // Otherwise sleep until the next real deadline, or block on the
        // channel when nothing is due (PTY/API/client/signal wake the loop).
        let now = Instant::now();
        let rearm_interval = if app.has_history_maintenance() {
            Duration::from_millis(1)
        } else {
            REARM_INTERVAL
        };
        let persist_due = !app.session_save_inflight
            && ((app.persist_session_now && !immediate_save_attempted)
                || (app.session_dirty && last_save.elapsed() >= SESSION_SAVE_DEBOUNCE));
        let rearm_due = app.has_pending_pty_output() && last_rearm.elapsed() >= rearm_interval;
        let received = if persist_due || rearm_due {
            // Already-due persist/re-arm must not `recv_timeout(0)`: that busy-loops
            // until the 100ms re-arm cadence elapses.
            None
        } else if render_request.needs_render() && has_render_clients(&clients) {
            match rx.recv_timeout(frame_wait(last_render_attempt.elapsed())) {
                Ok(ev) => Some(ev),
                Err(RecvTimeoutError::Timeout) => {
                    LOOP_DEADLINE_WAKES.fetch_add(1, Ordering::Relaxed);
                    None
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        } else {
            let mut deadline = app.next_runtime_deadline(now, has_active_clients(&clients));
            if app.session_dirty && !app.session_save_inflight {
                App::sooner_deadline(&mut deadline, last_save + SESSION_SAVE_DEBOUNCE);
            }
            if app.has_pending_pty_output() {
                App::sooner_deadline(&mut deadline, last_rearm + rearm_interval);
            }
            match deadline.map(|at| at.saturating_duration_since(now)) {
                Some(timeout) if !timeout.is_zero() => match rx.recv_timeout(timeout) {
                    Ok(ev) => Some(ev),
                    Err(RecvTimeoutError::Timeout) => {
                        LOOP_DEADLINE_WAKES.fetch_add(1, Ordering::Relaxed);
                        None
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                },
                Some(_) => None,
                None if shutdown::wake_available() => match rx.recv() {
                    Ok(ev) => Some(ev),
                    Err(_) => break,
                },
                None => match rx.recv_timeout(Duration::from_secs(1)) {
                    Ok(ev) => Some(ev),
                    Err(RecvTimeoutError::Timeout) => {
                        LOOP_DEADLINE_WAKES.fetch_add(1, Ordering::Relaxed);
                        None
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                },
            }
        };
        if let Some(ev) = received {
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
        let debounced_save_due = app.session_dirty && last_save.elapsed() >= SESSION_SAVE_DEBOUNCE;
        if !app.session_save_inflight && (immediate_save_due || debounced_save_due) {
            immediate_save_attempted = app.persist_session_now;
            app.schedule_session_save();
            last_save = Instant::now();
        }
        if app.detach_requested {
            app.detach_requested = false;
            if let Some(id) = foreground.take() {
                if let Some(c) = clients.remove(&id) {
                    let _ = c.send_control(ServerMessage::Detach);
                }
                foreground = latest_client(&clients);
                apply_foreground_client(&mut app, &clients, foreground);
                render_request.record(RenderCause::UserInterface);
            }
        }
        if let Some(name) = app.pending_session_switch.take() {
            if let Some(id) = foreground {
                if let Some(client) = clients.get(&id) {
                    let _ = client.send_control(ServerMessage::SwitchSession { name });
                }
            } else {
                app.show_toast("no attached client to switch".to_string());
            }
        }
        if std::mem::take(&mut app.pending_machine_selector) {
            if let Some(client) = foreground.and_then(|id| clients.get(&id)) {
                let _ = client.send_control(ServerMessage::OpenMachineSelector);
            } else {
                app.show_toast("no attached client to open the machine selector".to_string());
            }
        }
        if std::mem::take(&mut app.pending_machine_create) {
            if let Some(client) = foreground.and_then(|id| clients.get(&id)) {
                if client.machine_capable {
                    let area = Rect::new(0, 0, client.size.0, client.size.1);
                    let modal = app
                        .picker_rects
                        .iter()
                        .find_map(|(hit, rect)| {
                            matches!(hit, crate::app::PickerHit::Modal).then_some(*rect)
                        })
                        .unwrap_or_else(|| ui::workspace_picker_modal_rect(area, app.compact));
                    let _ = client.send_control(ServerMessage::OpenMachineCreate {
                        theme: protocol::MachineFormTheme {
                            surface: protocol::pack(app.theme.surface0),
                            border: protocol::pack(app.theme.border_focus),
                            text: protocol::pack(app.theme.text),
                            subtext0: protocol::pack(app.theme.subtext0),
                            subtext1: protocol::pack(app.theme.subtext1),
                            accent: protocol::pack(app.theme.accent),
                            accent_text: protocol::pack(app.theme.crust),
                            divider: protocol::pack(app.theme.overlay0),
                            rule: protocol::pack(app.theme.surface1),
                            error: protocol::pack(app.theme.coral),
                        },
                        modal: protocol::ShellDockBlock {
                            x: modal.x,
                            y: modal.y,
                            width: modal.width,
                            height: modal.height,
                        },
                    });
                } else {
                    app.show_toast("this client cannot add remote machines".to_string());
                }
            } else {
                app.show_toast("no attached client to add a remote machine".to_string());
            }
        }
        if let Some(revision) = app.pending_machine_catalog_revision.take() {
            broadcast_machine_catalog_changed(&mut clients, revision);
        }

        // A state transition here (e.g. a silent agent reaching Done) has no PtyData
        // to ride on, so repaint when detection reports a visible change.
        let now = Instant::now();
        if app.detect_tick_with(now, has_active_clients(&clients)) {
            render_request.record(RenderCause::Detection);
        }
        // Parked `wait.output` deadlines lapse on the tick (docs/81); a no-op
        // while nobody is waiting.
        app.tick_output_waits(now);
        app.tick_agent_waits(now);
        app.tick_agent_workflows(now);
        app.tick_backend_revision_waits(now);
        if app.tick_automations(crate::automation::unix_now()) {
            render_request.record(RenderCause::Detection);
        }
        for msg in app.pending_notify.drain(..) {
            broadcast_effect(&mut clients, ServerMessage::Notify(msg));
        }
        if let Some(signal) = app.pending_sound.take() {
            broadcast_effect(&mut clients, ServerMessage::Sound(signal));
        }
        // A finished mouse selection copies to the client's clipboard (OSC 52).
        if let Some(url) = app.pending_open_url.take() {
            broadcast_effect(&mut clients, ServerMessage::OpenUrl(url));
        }
        // `pending_open_path` is never broadcast: it is routed to the one client
        // whose input produced it, inside `apply`. Nothing else sets it today;
        // if something ever did, dropping it here beats opening the path on a
        // desktop that never asked for it.
        app.pending_open_path = None;
        if let Some(text) = app.pending_clipboard.take() {
            broadcast_effect(&mut clients, ServerMessage::Clipboard(text));
        }
        // An expired toast forces one render so it disappears (idle frames don't).
        if app.tick_toast(Instant::now()) {
            render_request.record(RenderCause::Metadata);
        }
        if app.tick_copy_highlight(Instant::now()) {
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
        if last_rearm.elapsed() >= rearm_interval {
            last_rearm = Instant::now();
            let (visible, background, title_changed) = app.rearm_pty_notify_by_visibility();
            if title_changed {
                render_request.record(RenderCause::Metadata);
            }
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
        let any_behind = clients
            .values()
            .any(|client| client.interest != SurfaceInterest::Suspended && client.behind);
        if app.force_redraw {
            render_request.record(RenderCause::ForcedRepair);
        }
        if any_behind {
            render_request.record(RenderCause::ClientResync);
        }

        let projection_causes = RenderCause::UserInterface.bit()
            | RenderCause::ApiOrMaintenance.bit()
            | RenderCause::Metadata.bit()
            | RenderCause::ClientAttachOrResize.bit();
        let refresh_navigation = render_request.causes & projection_causes != 0;
        if render_request.needs_render()
            && has_render_clients(&clients)
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
            let (visible, background, title_changed) = app.rearm_pty_notify_by_visibility();
            if title_changed {
                render_request.record(RenderCause::Metadata);
            }
            if visible {
                render_request.record_visible_pty();
            }
            if background {
                render_request.record_hidden_pty();
            }
        }
        // Suspended clients retain semantic navigation, not terminal frames.
        // Refresh only after a non-PTY change and send only changed snapshots.
        if refresh_navigation {
            refresh_suspended_workspaces(&app, &mut clients);
        }
        if !has_render_clients(&clients) {
            // Suspended remote endpoints retain no render debt. Preparing one
            // later always starts with a complete fresh projection.
            render_request.clear();
            app.force_redraw = false;
        }
    }

    app.finish_session_persistence();
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
                    // The client reports cell pixels in a separate post-handshake
                    // message; until it lands, auto-split uses the documented
                    // fallback aspect.
                    0,
                    0,
                    terminal_colors,
                    activity,
                ),
            );
            *foreground = Some(id);
            apply_foreground_client(app, clients, *foreground);
            app.mark_runtime_scans_dirty();
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
            app.client_files_visible = client_files_visible(clients);
            if was_foreground {
                *foreground = latest_client(clients);
                apply_foreground_client(app, clients, *foreground);
            }
            was_foreground
        }
        AppEvent::ClientPrepareSurface {
            id,
            ticket,
            cols,
            rows,
        } => {
            let Some(client) = clients.get_mut(&id) else {
                return false;
            };
            client.prepare_ticket = Some(ticket);
            client.size = (cols.max(1), rows.max(1));
            client.interest = SurfaceInterest::Prepared;
            client.force_full = true;
            client.retained_ready = false;
            client.retained_pane_content.clear();
            if *foreground == Some(id) {
                *foreground = latest_client(clients);
                apply_foreground_client(app, clients, *foreground);
            }
            true
        }
        AppEvent::ClientSurfaceInterest { id, interest } => {
            let Some(client) = clients.get_mut(&id) else {
                return false;
            };
            if client.interest == interest {
                return false;
            }
            client.interest = interest;
            client.prepare_ticket = None;
            client.force_full = true;
            client.behind = false;
            client.retained_ready = false;
            client.retained_pane_content.clear();
            if interest == SurfaceInterest::Prepared && client.shell_dock_layout.rows > 0 {
                client.shell_dock_dirty = true;
            }
            if interest == SurfaceInterest::Suspended {
                client.last_frame = None;
                client.render_buf = Buffer::empty(Rect::new(0, 0, 1, 1));
            }
            if interest == SurfaceInterest::Active {
                client.last_activity = *next_activity;
                *next_activity = next_activity.saturating_add(1);
                *foreground = Some(id);
                apply_foreground_client(app, clients, *foreground);
            } else if *foreground == Some(id) {
                *foreground = latest_client(clients);
                apply_foreground_client(app, clients, *foreground);
            }
            app.client_files_visible = client_files_visible(clients);
            true
        }
        AppEvent::ClientShellSidebars { id, mut state } => {
            let Some(client) = clients.get_mut(&id) else {
                return false;
            };
            if !client.machine_capable || !client.shell_dock_layout.owns_workspaces {
                return false;
            }
            if client
                .shell_sidebars
                .as_ref()
                .is_some_and(|current| current.revision > state.revision)
            {
                return false;
            }
            // Reuse the native cap, width clamp, and weight normalization.
            state.layout = crate::app::Sidebars::from_config(&state.layout).to_config();
            if client.shell_sidebars.as_ref() == Some(&state) {
                return false;
            }
            client.sidebar_cache = Some(crate::app::Sidebars::from_config(&state.layout));
            client.shell_sidebars = Some(state);
            // Layout still needs a complete render pass, but the existing
            // client frame remains a valid diff baseline. Sending a full frame
            // for every client-owned drag update made remote resizing much
            // heavier than the native local path.
            client.retained_ready = false;
            true
        }
        AppEvent::ClientShellDockLayout { id, mut layout } => {
            let Some(client) = clients.get_mut(&id) else {
                return false;
            };
            layout.rows = layout.rows.min(12);
            layout.workspace_width = layout
                .workspace_width
                .or(client.shell_dock_layout.workspace_width)
                .map(|width| {
                    width.clamp(crate::app::SIDEBAR_WIDTH_MIN, crate::app::SIDEBAR_WIDTH_MAX)
                });
            if layout.rows == 0 {
                layout.leading = false;
                layout.indent_workspaces = false;
            }
            let changed = client.shell_dock_layout != layout || !client.machine_capable;
            client.machine_capable = true;
            if !changed {
                return false;
            }
            client.shell_dock_layout = layout;
            client.force_full = true;
            client.retained_ready = false;
            client.shell_dock_dirty = layout.rows > 0 || client.last_shell_dock.is_some();
            true
        }
        AppEvent::ClientShellWorkspaceFocus { id, workspace_id } => {
            let Some(client) = clients.get(&id) else {
                return false;
            };
            if !client.machine_capable {
                return false;
            }
            let Some(index) = app
                .workspaces
                .iter()
                .position(|workspace| workspace.id == workspace_id)
            else {
                return false;
            };
            app.active_ws = index;
            app.sidebar_focus = None;
            app.files_focused = false;
            true
        }
        AppEvent::ClientShellWorkspaceMenu {
            id,
            workspace_id,
            col,
            row,
        } => {
            let Some(client) = clients.get(&id) else {
                return false;
            };
            if !client.machine_capable || client.interest != SurfaceInterest::Active {
                return false;
            }
            let Some(index) = app
                .workspaces
                .iter()
                .position(|workspace| workspace.id == workspace_id)
            else {
                return false;
            };
            app.files_focused = false;
            app.open_ws_menu(
                index,
                col.min(client.size.0.saturating_sub(1)),
                row.min(client.size.1.saturating_sub(1)),
            );
            true
        }
        AppEvent::ClientOpenWorkspacePicker { id } => {
            let Some(client) = clients.get(&id) else {
                return false;
            };
            if !client.machine_capable {
                return false;
            }
            app.open_folder_picker();
            true
        }
        AppEvent::ClientCellPixels {
            id,
            cell_width_px,
            cell_height_px,
        } => {
            let Some(client) = clients.get_mut(&id) else {
                return false;
            };
            client.cell_width_px = cell_width_px;
            client.cell_height_px = cell_height_px;
            // Passive metadata: only the interactive client's display may define
            // the geometry shared state uses, and it never forces a repaint.
            if *foreground == Some(id) {
                app.set_client_cell_pixels(cell_width_px, cell_height_px);
            }
            false
        }
        AppEvent::ClientInput { id, input } => {
            let Some(client) = clients.get_mut(&id) else {
                discard_client_input(input);
                return false;
            };
            if client.interest != SurfaceInterest::Active {
                // A prepared channel may update only its candidate viewport.
                if let (
                    SurfaceInterest::Prepared,
                    ClientInput::Resize {
                        cols,
                        rows,
                        cell_width_px,
                        cell_height_px,
                    },
                ) = (client.interest, &input)
                {
                    client.size = ((*cols).max(1), (*rows).max(1));
                    client.cell_width_px = *cell_width_px;
                    client.cell_height_px = *cell_height_px;
                    client.force_full = true;
                    return true;
                }
                discard_client_input(input);
                return false;
            }
            client.last_activity = *next_activity;
            *next_activity = next_activity.saturating_add(1);

            if let ClientInput::Resize {
                cols,
                rows,
                cell_width_px,
                cell_height_px,
            } = input
            {
                crate::logging::event(
                    crate::logging::EventKind::ServerClientResize,
                    &[
                        crate::logging::Field::ClientId(id),
                        crate::logging::Field::Cols(u64::from(cols)),
                        crate::logging::Field::Rows(u64::from(rows)),
                    ],
                );
                client.size = (cols.max(1), rows.max(1));
                client.cell_width_px = cell_width_px;
                client.cell_height_px = cell_height_px;
                if *foreground == Some(id) {
                    app.set_client_cell_pixels(cell_width_px, cell_height_px);
                }
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
                apply_foreground_client(app, clients, *foreground);
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
                    apply_foreground_client(app, clients, *foreground);
                    discard_client_input(input);
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
                ClientInput::PasteImage(path) => AppEvent::PasteImage(path),
                ClientInput::Resize { .. } => unreachable!("handled above"),
            };
            let client = clients
                .get_mut(&id)
                .expect("input client remains registered");
            let scoped = client.machine_capable && client.shell_dock_layout.owns_workspaces;
            let changed = if !scoped {
                app.handle_event(event)
            } else {
                let previous = app.sidebars.clone();
                let previous_workspace_paths = app.config.layout.workspace_paths;
                if let Some(state) = &client.shell_sidebars {
                    app.sidebars = crate::app::Sidebars::from_config(&state.layout);
                    app.config.layout.workspace_paths = state.workspace_paths;
                }
                app.client_sidebar_input = true;
                let changed = app.handle_event(event);
                app.client_sidebar_input = false;
                let layout = app.sidebars.to_config();
                let workspace_paths = app.config.layout.workspace_paths;
                app.sidebars = previous;
                app.config.layout.workspace_paths = previous_workspace_paths;
                let revision = client.shell_sidebars.as_ref().map_or(1, |state| {
                    state.revision.saturating_add(u64::from(
                        state.layout != layout || state.workspace_paths != workspace_paths,
                    ))
                });
                client.shell_sidebars = Some(protocol::ShellSidebars {
                    revision,
                    layout,
                    workspace_paths,
                });
                client.sidebar_cache = client
                    .shell_sidebars
                    .as_ref()
                    .map(|state| crate::app::Sidebars::from_config(&state.layout));
                changed
            };
            // Route desktop effects to the client whose input produced them,
            // after restoring shared sidebar state for machine-aware clients.
            if let Some(path) = app.pending_open_path.take() {
                let gone = clients.get(&id).is_some_and(|client| {
                    client
                        .sender
                        .send_control(ServerMessage::OpenPath(path))
                        .is_err()
                });
                if gone {
                    clients.remove(&id);
                    if *foreground == Some(id) {
                        *foreground = latest_client(clients);
                    }
                    apply_foreground_client(app, clients, *foreground);
                }
            }
            changed
        }
        // Redraw only if the event actually changed the UI — a plain keystroke
        // forwarded to a pane does not (its echo arrives as a separate `PtyData`).
        other => app.handle_event(other),
    }
}

fn discard_client_input(input: ClientInput) {
    if let ClientInput::PasteImage(path) = input {
        crate::clipboard_image::discard_staged_png(&path);
    }
}

fn broadcast(clients: &mut Clients, msg: ServerMessage) {
    clients.retain(|_, client| client.send_control(msg.clone()).is_ok());
}

fn broadcast_effect(clients: &mut Clients, msg: ServerMessage) {
    clients.retain(|_, client| {
        client.interest != SurfaceInterest::Active || client.send_control(msg.clone()).is_ok()
    });
}

fn broadcast_machine_catalog_changed(clients: &mut Clients, revision: u64) {
    clients.retain(|_, client| {
        !client.machine_capable
            || client
                .send_control(ServerMessage::MachineCatalogChanged { revision })
                .is_ok()
    });
}

fn has_active_clients(clients: &Clients) -> bool {
    clients
        .values()
        .any(|client| client.interest == SurfaceInterest::Active)
}

fn has_render_clients(clients: &Clients) -> bool {
    clients
        .values()
        .any(|client| client.interest != SurfaceInterest::Suspended)
}

fn latest_client(clients: &Clients) -> Option<u64> {
    clients
        .iter()
        .filter(|(_, client)| client.interest == SurfaceInterest::Active)
        .max_by_key(|(_, client)| client.last_activity)
        .map(|(&id, _)| id)
}

/// Adopt the foreground client's per-display state. Cell pixels drive automatic
/// split geometry; the palette only matters for the `terminal` theme.
fn apply_foreground_client(app: &mut App, clients: &Clients, foreground: Option<u64>) {
    let Some(client) = foreground.and_then(|id| clients.get(&id)) else {
        return;
    };
    app.set_client_cell_pixels(client.cell_width_px, client.cell_height_px);
    if app.config.theme != "terminal" {
        return;
    }
    if let Some(colors) = client.terminal_colors.as_ref() {
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
        AppEvent::PtyData(id) if app.hidden_title_changed(*id) => {
            EventRenderSource::Cause(RenderCause::Metadata)
        }
        AppEvent::PtyData(_) => EventRenderSource::HiddenPty,
        AppEvent::ClientConnected { .. }
        | AppEvent::ClientInput {
            input: ClientInput::Resize { .. },
            ..
        } => EventRenderSource::Cause(RenderCause::ClientAttachOrResize),
        // Cell-size metadata changes future split geometry, never the current
        // frame. `apply` reports no damage, so this never asks for a render.
        AppEvent::ClientCellPixels { .. } => EventRenderSource::Cause(RenderCause::Metadata),
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
fn client_files_visible(clients: &Clients) -> bool {
    clients.values().any(|client| {
        client.interest != SurfaceInterest::Suspended
            && client.shell_dock_layout.owns_workspaces
            && client.shell_sidebars.as_ref().is_some_and(|state| {
                [&state.layout.left, &state.layout.right]
                    .iter()
                    .any(|side| side.visible && side.docks.iter().any(|dock| dock == "files"))
            })
    })
}

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
    app.client_files_visible = client_files_visible(clients);
    if !clients
        .values()
        .any(|client| client.interest != SurfaceInterest::Suspended)
    {
        return false;
    }
    if foreground.is_none_or(|id| !clients.contains_key(&id)) {
        *foreground = latest_client(clients);
        apply_foreground_client(app, clients, *foreground);
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
        if clients
            .get(&id)
            .is_some_and(|client| client.interest == SurfaceInterest::Suspended)
        {
            continue;
        }
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
        apply_foreground_client(app, clients, *foreground);
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

fn refresh_suspended_workspaces(app: &App, clients: &mut Clients) {
    if !clients
        .values()
        .any(|client| client.machine_capable && client.interest == SurfaceInterest::Suspended)
    {
        return;
    }
    let projection = shell_workspace_projection(app);
    clients.retain(|_, client| {
        if client.machine_capable
            && client.interest == SurfaceInterest::Suspended
            && client.last_shell_workspaces != projection
        {
            if client
                .send_control(ServerMessage::ShellWorkspaces(projection.clone()))
                .is_err()
            {
                return false;
            }
            client.last_shell_workspaces.clone_from(&projection);
        }
        true
    });
}

fn shell_resize_projection(
    rect: Rect,
    left_seam: Option<Rect>,
    right_seam: Option<Rect>,
    main: Rect,
    pane: Rect,
) -> Option<protocol::ShellResize> {
    let left = left_seam.is_some_and(|seam| seam.x == rect.right().saturating_sub(1));
    let seam = if left { left_seam? } else { right_seam? };
    let other = if left {
        right_seam.map_or(0, |s| main.right().saturating_sub(s.x))
    } else {
        left_seam.map_or(0, |s| s.x.saturating_sub(main.x).saturating_add(1))
    };
    Some(protocol::ShellResize {
        left,
        column: seam.x,
        top: pane.y.max(seam.y),
        bottom: pane.bottom().min(seam.bottom()),
        origin: if left { main.x } else { main.right() },
        maximum: main
            .width
            .saturating_sub(24 + other)
            .clamp(crate::app::SIDEBAR_WIDTH_MIN, crate::app::SIDEBAR_WIDTH_MAX),
    })
}

fn shell_workspace_projection(app: &App) -> Vec<protocol::ShellWorkspace> {
    let order = app.workspace_display_order();
    order
        .iter()
        .take(256)
        .filter_map(|(index, nested)| {
            let workspace = app.workspaces.get(*index)?;
            let state = ui::sidebar::rollup(app, *index);
            Some(protocol::ShellWorkspace {
                dot: state.dot().to_string(),
                dot_color: protocol::pack(state.color(&app.theme)),
                id: workspace.id.clone(),
                index: u16::try_from(*index).ok()?,
                name: workspace.name.clone(),
                cwd: ui::short_path(
                    app.workspace_terminal_cwd(*index).unwrap_or(&workspace.cwd),
                    u16::MAX,
                ),
                branch: workspace.branch.clone(),
                active: *index == app.active_ws,
                selected: app.sidebar_focus == Some(crate::app::SidebarListFocus::Workspaces)
                    && order
                        .get(app.workspace_cursor)
                        .is_some_and(|(selected, _)| selected == index),
                nested: *nested,
            })
        })
        .collect()
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

    let previous_shell_rows = app.client_shell_dock_rows;
    let previous_workspace_paths = app.config.layout.workspace_paths;
    let scoped_sidebars = client.machine_capable && client.shell_dock_layout.owns_workspaces;
    if scoped_sidebars {
        if client.shell_sidebars.is_none() {
            let mut layout = app.sidebars.to_config();
            if let Some(width) = client.shell_dock_layout.workspace_width {
                for side in [&mut layout.left, &mut layout.right] {
                    if side.docks.iter().any(|dock| dock == "workspaces") {
                        side.width = width;
                    }
                }
            }
            client.shell_sidebars = Some(protocol::ShellSidebars {
                revision: 0,
                layout,
                workspace_paths: app.config.layout.workspace_paths,
            });
        }
        let sidebars = client.sidebar_cache.get_or_insert_with(|| {
            crate::app::Sidebars::from_config(
                &client
                    .shell_sidebars
                    .as_ref()
                    .expect("initialized layout")
                    .layout,
            )
        });
        std::mem::swap(&mut app.sidebars, sidebars);
        app.config.layout.workspace_paths = client
            .shell_sidebars
            .as_ref()
            .expect("initialized shell state")
            .workspace_paths;
    }
    let previous_shell_leading = app.client_shell_dock_leading;
    let previous_workspace_indent = app.client_shell_dock_indent_workspaces;
    let previous_machine_capable = app.client_machine_capable;
    let previous_shell_owner = app.client_shell_owns_workspaces;
    let previous_session_chrome_owner = app.client_shell_owns_session_chrome;
    let previous_shell_rect = app.client_shell_dock_rect;
    app.client_shell_dock_rows = client.shell_dock_layout.rows;
    app.client_shell_dock_leading = client.shell_dock_layout.leading;
    app.client_shell_dock_indent_workspaces = client.shell_dock_layout.indent_workspaces;
    app.client_machine_capable = client.machine_capable;
    app.client_shell_owns_workspaces = client.shell_dock_layout.owns_workspaces;
    app.client_shell_owns_session_chrome = client.shell_dock_layout.owns_session_chrome;
    app.client_shell_dock_rect = None;
    let (cursor, cursor_visible, shell_dock, shell_workspaces) = if let Some(cursor) = patched {
        PARTIAL_TERMINAL_PROJECTIONS.fetch_add(1, Ordering::Relaxed);
        (
            cursor.0,
            cursor.1,
            client.last_shell_dock,
            client.last_shell_workspaces.clone(),
        )
    } else {
        if partial_pass {
            RETAINED_RENDER_FALLBACKS.fetch_add(1, Ordering::Relaxed);
        }
        FULL_TERMINAL_PROJECTIONS.fetch_add(1, Ordering::Relaxed);
        client.render_buf.reset();
        let mut target = ui::RenderTarget::new(&mut client.render_buf, area);
        let (shell_overlay, session_slot, session_button, shell_geometry) = if interactive {
            ui::render_into(&mut target, app);
            client
                .retained_pane_content
                .clone_from(&app.pane_content_rects);
            client.retained_ready = true;
            (
                ui::shell_overlay_rect(app, area),
                app.named_session_slot_rect,
                app.named_session_button_rect,
                (
                    app.left_seam,
                    app.right_seam,
                    app.last_main_area,
                    app.last_pane_area,
                ),
            )
        } else {
            let projection = ui::render_projection(&mut target, app);
            client.retained_pane_content = projection.pane_content;
            client.retained_ready = true;
            app.client_shell_dock_rect = projection.shell_dock;
            (
                projection.shell_overlay,
                projection.session_slot,
                projection.session_button,
                (
                    projection.left_seam,
                    projection.right_seam,
                    projection.main_area,
                    projection.pane_area,
                ),
            )
        };
        (
            target.cursor(),
            target.cursor_visible(),
            app.client_shell_dock_rect
                .map(|rect| protocol::ShellDockRect {
                    resize: shell_resize_projection(
                        rect,
                        shell_geometry.0,
                        shell_geometry.1,
                        shell_geometry.2,
                        shell_geometry.3,
                    ),
                    session_slot: session_slot.map(|slot| protocol::ShellDockBlock {
                        x: slot.x,
                        y: slot.y,
                        width: slot.width,
                        height: slot.height,
                    }),
                    session_button: session_button.map(|button| protocol::ShellDockBlock {
                        x: button.x,
                        y: button.y,
                        width: button.width,
                        height: button.height,
                    }),
                    overlay: shell_overlay.map(|overlay| protocol::ShellDockBlock {
                        x: overlay.x,
                        y: overlay.y,
                        width: overlay.width,
                        height: overlay.height,
                    }),
                    overlay_dims_background: app.picker.is_some(),
                    workspace_focused: app.sidebar_focus
                        == Some(crate::app::SidebarListFocus::Workspaces),
                    workspace_modal: app.mode != crate::app::Mode::Normal
                        || app.ws_menu.is_some()
                        || app.ws_rename.is_some()
                        || app.named_session_menu.is_some(),
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: rect.height,
                    show_paths: app.config.layout.workspace_paths,
                    normal_fg: protocol::pack(app.theme.subtext1),
                    secondary_fg: protocol::pack(app.theme.overlay0),
                    active_fg: protocol::pack(app.theme.accent),
                    active_secondary_fg: protocol::pack(app.theme.subtext0),
                    active_bg: protocol::pack(app.theme.sel_bg),
                    branch_fg: protocol::pack(app.theme.green),
                    chrome: protocol::MachineFormTheme {
                        surface: protocol::pack(app.theme.surface0),
                        border: protocol::pack(app.theme.border_focus),
                        text: protocol::pack(app.theme.text),
                        subtext0: protocol::pack(app.theme.subtext0),
                        subtext1: protocol::pack(app.theme.subtext1),
                        accent: protocol::pack(app.theme.accent),
                        accent_text: protocol::pack(app.theme.crust),
                        divider: protocol::pack(app.theme.overlay0),
                        rule: protocol::pack(app.theme.surface1),
                        error: protocol::pack(app.theme.coral),
                    },
                }),
            shell_workspace_projection(app),
        )
    };
    if scoped_sidebars {
        std::mem::swap(
            &mut app.sidebars,
            client
                .sidebar_cache
                .as_mut()
                .expect("initialized layout cache"),
        );
        app.config.layout.workspace_paths = previous_workspace_paths;
    }
    if !interactive {
        app.client_shell_dock_rows = previous_shell_rows;
        app.client_shell_dock_leading = previous_shell_leading;
        app.client_shell_dock_indent_workspaces = previous_workspace_indent;
        app.client_machine_capable = previous_machine_capable;
        app.client_shell_owns_workspaces = previous_shell_owner;
        app.client_shell_owns_session_chrome = previous_session_chrome_owner;
        app.client_shell_dock_rect = previous_shell_rect;
    }
    if client.shell_sidebars != client.last_shell_sidebars {
        if let Some(state) = &client.shell_sidebars {
            if client
                .send_control(ServerMessage::ShellSidebars(state.clone()))
                .is_err()
            {
                return RenderClientOutcome {
                    enqueued: false,
                    disconnected: true,
                };
            }
        }
        client
            .last_shell_sidebars
            .clone_from(&client.shell_sidebars);
    }
    if client.machine_capable && shell_workspaces != client.last_shell_workspaces {
        if client
            .send_control(ServerMessage::ShellWorkspaces(shell_workspaces.clone()))
            .is_err()
        {
            return RenderClientOutcome {
                enqueued: false,
                disconnected: true,
            };
        }
        client.last_shell_workspaces = shell_workspaces;
    }
    if client.shell_dock_dirty || shell_dock != client.last_shell_dock {
        if client
            .send_control(ServerMessage::ShellDock(shell_dock))
            .is_err()
        {
            return RenderClientOutcome {
                enqueued: false,
                disconnected: true,
            };
        }
        client.last_shell_dock = shell_dock;
        client.shell_dock_dirty = false;
    }

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
    let message = match (client.prepare_ticket, message) {
        (Some(ticket), ServerMessage::Frame(frame)) => {
            ServerMessage::PreparedFrame { ticket, frame }
        }
        (_, message) => message,
    };
    CHANGED_PROJECTIONS.fetch_add(1, Ordering::Relaxed);
    match client.sender.try_send_frame(message) {
        Ok(()) => {
            FRAMES_ENQUEUED.fetch_add(1, Ordering::Relaxed);
            client.prepare_ticket = None;
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

fn ends_client_writer(message: &ServerMessage) -> bool {
    matches!(
        message,
        ServerMessage::Detach | ServerMessage::ServerShutdown { .. }
    )
}

fn handle_client(id: u64, stream: Conn, app_tx: Sender<AppEvent>, terminal_theme: Arc<AtomicBool>) {
    let mut reader = BufReader::new(stream.clone());
    let mut writer = stream;

    // Reject with a reason the user can act on. `Hello` keeps a frozen wire shape
    // so every released client decodes here, but a corrupt or truly foreign frame
    // still has to answer with the mismatch error rather than hanging up silently:
    // the client turns a dropped socket into an opaque IO error.
    let reject_version = |writer: &mut Conn, version: Option<u32>| {
        crate::logging::event(
            crate::logging::EventKind::ServerClientHandshakeRejected,
            &[
                crate::logging::Field::Reason(crate::logging::Reason::VersionMismatch),
                crate::logging::Field::ProtocolVersion(u64::from(version.unwrap_or(0))),
            ],
        );
        let _ = protocol::write_message(
            writer,
            &ServerMessage::Welcome {
                version: protocol::PROTOCOL_VERSION,
                error: Some("protocol version mismatch".into()),
            },
        );
    };

    let (cols, rows) = match protocol::read_message::<_, ClientMessage>(&mut reader) {
        Ok(ClientMessage::Hello {
            version,
            cols,
            rows,
        }) => {
            if version != protocol::PROTOCOL_VERSION {
                reject_version(&mut writer, Some(version));
                return;
            }
            (cols, rows)
        }
        Ok(_) => {
            reject_version(&mut writer, None);
            return;
        }
        Err(_) => {
            reject_version(&mut writer, None);
            return;
        }
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
    if protocol::write_message(
        &mut writer,
        &ServerMessage::EndpointIdentity {
            boot_id: server_boot_id(),
            session: crate::session::display_name(),
        },
    )
    .is_err()
    {
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
    let health_tx = message_tx.clone();
    let frame_pending = Arc::new(AtomicBool::new(false));
    let writer_frame_pending = frame_pending.clone();
    thread::spawn(move || {
        for msg in message_rx {
            let frame_stats = match &msg {
                ServerMessage::Frame(_) | ServerMessage::PreparedFrame { .. } => {
                    Some((true, 0usize))
                }
                ServerMessage::FrameDiff(frame) => Some((false, frame.runs.len())),
                _ => None,
            };
            if frame_stats.is_some() {
                // Match sync_channel(1): receiving frees the single frame slot,
                // even while the socket write itself is still in progress.
                writer_frame_pending.store(false, Ordering::Release);
            }
            let stop = ends_client_writer(&msg);
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
            Ok(ClientMessage::ClipboardImage(png)) => {
                let Ok(path) = crate::clipboard_image::stage_png(&png) else {
                    continue;
                };
                if app_tx
                    .send(AppEvent::ClientInput {
                        id,
                        input: ClientInput::PasteImage(path.clone()),
                    })
                    .is_err()
                {
                    crate::clipboard_image::discard_staged_png(&path);
                    break;
                }
            }
            Ok(ClientMessage::Resize {
                cols,
                rows,
                cell_width_px,
                cell_height_px,
            }) => {
                if app_tx
                    .send(AppEvent::ClientInput {
                        id,
                        input: ClientInput::Resize {
                            cols,
                            rows,
                            cell_width_px,
                            cell_height_px,
                        },
                    })
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMessage::CellPixels {
                cell_width_px,
                cell_height_px,
            }) => {
                if app_tx
                    .send(AppEvent::ClientCellPixels {
                        id,
                        cell_width_px,
                        cell_height_px,
                    })
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMessage::PrepareSurface { ticket, cols, rows }) => {
                if app_tx
                    .send(AppEvent::ClientPrepareSurface {
                        id,
                        ticket,
                        cols,
                        rows,
                    })
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMessage::SurfaceInterest(interest)) => {
                if app_tx
                    .send(AppEvent::ClientSurfaceInterest { id, interest })
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMessage::ShellSidebars(state)) => {
                if state.layout.left.docks.len() > crate::app::MAX_DOCKS_PER_SIDE
                    || state.layout.right.docks.len() > crate::app::MAX_DOCKS_PER_SIDE
                    || state
                        .layout
                        .left
                        .docks
                        .iter()
                        .chain(&state.layout.right.docks)
                        .any(|id| id.len() > 256)
                    || state.layout.left.dock_weights.len() > crate::app::MAX_DOCKS_PER_SIDE
                    || state.layout.right.dock_weights.len() > crate::app::MAX_DOCKS_PER_SIDE
                {
                    break;
                }
                if app_tx
                    .send(AppEvent::ClientShellSidebars { id, state })
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMessage::ShellDockLayout(layout)) => {
                if app_tx
                    .send(AppEvent::ClientShellDockLayout { id, layout })
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMessage::ShellWorkspaceFocus { workspace_id }) => {
                if workspace_id.len() > 256 {
                    break;
                }
                if app_tx
                    .send(AppEvent::ClientShellWorkspaceFocus { id, workspace_id })
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMessage::ShellWorkspaceMenu {
                workspace_id,
                col,
                row,
            }) => {
                if workspace_id.len() > 256 {
                    break;
                }
                if app_tx
                    .send(AppEvent::ClientShellWorkspaceMenu {
                        id,
                        workspace_id,
                        col,
                        row,
                    })
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMessage::OpenWorkspacePicker) => {
                if app_tx
                    .send(AppEvent::ClientOpenWorkspacePicker { id })
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMessage::HealthCheck { nonce }) => {
                if health_tx
                    .send(ServerMessage::HealthCheck { nonce })
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

fn server_boot_id() -> u64 {
    static BOOT_ID: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *BOOT_ID.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        nanos.rotate_left(17) ^ u64::from(std::process::id())
    })
}

/// Graceful shutdown on a termination signal. The handler only flips an atomic
/// and writes a self-pipe (async-signal-safe); a waiter thread then posts
/// `AppEvent::Shutdown` so a sleeping event loop still exits through the normal
/// path — clients notified, the session saved — instead of dying mid-state on
/// SIGTERM (logout, `kill`, system shutdown).
#[cfg(unix)]
mod shutdown {
    use std::io;
    use std::os::fd::RawFd;
    use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
    use std::sync::mpsc::Sender;
    use std::thread;

    use crate::event::AppEvent;

    static FLAG: AtomicBool = AtomicBool::new(false);
    static WRITE_FD: AtomicI32 = AtomicI32::new(-1);
    static WAKE_AVAILABLE: AtomicBool = AtomicBool::new(false);

    pub fn requested() -> bool {
        FLAG.load(Ordering::Relaxed)
    }

    pub fn wake_available() -> bool {
        WAKE_AVAILABLE.load(Ordering::Relaxed)
    }

    pub fn install(tx: Sender<AppEvent>) {
        let mut fds = [-1, -1];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            install_handler();
            return;
        }
        if set_cloexec(fds[0]).is_err() || set_nonblocking_cloexec(fds[1]).is_err() {
            unsafe {
                libc::close(fds[0]);
                libc::close(fds[1]);
            }
            install_handler();
            return;
        }
        WRITE_FD.store(fds[1], Ordering::Relaxed);
        let read_fd = fds[0];
        install_handler();
        if thread::Builder::new()
            .name("luvus-signal".into())
            .spawn(move || wait_for_signal(read_fd, tx))
            .is_ok()
        {
            WAKE_AVAILABLE.store(true, Ordering::Relaxed);
        }
    }

    fn wait_for_signal(read_fd: RawFd, tx: Sender<AppEvent>) {
        let mut buf = [0u8; 8];
        loop {
            let count = unsafe { libc::read(read_fd, buf.as_mut_ptr().cast(), buf.len()) };
            if count > 0 {
                FLAG.store(true, Ordering::Relaxed);
                let _ = tx.send(AppEvent::Shutdown);
                continue;
            }
            if count < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
            }
            break;
        }
    }

    fn install_handler() {
        extern "C" fn on_signal(_sig: libc::c_int) {
            FLAG.store(true, Ordering::Relaxed);
            let fd = WRITE_FD.load(Ordering::Relaxed);
            if fd >= 0 {
                write_wake_preserving_errno(fd);
            }
        }
        unsafe {
            let h = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
            libc::signal(libc::SIGTERM, h);
            libc::signal(libc::SIGHUP, h);
            libc::signal(libc::SIGINT, h);
        }
    }

    fn write_wake_preserving_errno(fd: RawFd) {
        let byte = 1u8;
        unsafe {
            let errno = errno_location();
            let saved_errno = errno.as_ref().copied();
            let _ = libc::write(fd, (&byte as *const u8).cast(), 1);
            if let Some(saved_errno) = saved_errno {
                *errno = saved_errno;
            }
        }
    }

    #[cfg(any(
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "dragonfly"
    ))]
    unsafe fn errno_location() -> *mut libc::c_int {
        libc::__error()
    }

    #[cfg(any(target_os = "linux", target_os = "hurd", target_os = "redox"))]
    unsafe fn errno_location() -> *mut libc::c_int {
        libc::__errno_location()
    }

    #[cfg(any(
        target_os = "android",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "nuttx"
    ))]
    unsafe fn errno_location() -> *mut libc::c_int {
        libc::__errno()
    }

    #[cfg(any(target_os = "solaris", target_os = "illumos"))]
    unsafe fn errno_location() -> *mut libc::c_int {
        libc::___errno()
    }

    #[cfg(target_os = "aix")]
    unsafe fn errno_location() -> *mut libc::c_int {
        libc::_Errno()
    }

    // libc exposes no common errno accessor across every possible `unix`
    // target. Unknown targets retain an async-signal-safe handler; null
    // precisely disables save/restore instead of guessing an ABI symbol.
    #[cfg(not(any(
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "linux",
        target_os = "hurd",
        target_os = "redox",
        target_os = "android",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "nuttx",
        target_os = "solaris",
        target_os = "illumos",
        target_os = "aix"
    )))]
    unsafe fn errno_location() -> *mut libc::c_int {
        std::ptr::null_mut()
    }

    fn set_cloexec(fd: RawFd) -> io::Result<()> {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn set_nonblocking_cloexec(fd: RawFd) -> io::Result<()> {
        set_cloexec(fd)?;
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Windows: no POSIX signals; the detached server is stopped via `server stop`.
#[cfg(not(unix))]
mod shutdown {
    use std::sync::mpsc::Sender;

    use crate::event::AppEvent;

    pub fn requested() -> bool {
        false
    }

    pub fn wake_available() -> bool {
        // Stop arrives as an API event, not a POSIX signal.
        true
    }

    pub fn install(_tx: Sender<AppEvent>) {}
}

#[cfg(test)]
mod tests {
    use super::ServerMessage;
    use super::{
        apply, broadcast, broadcast_effect, broadcast_machine_catalog_changed, ends_client_writer,
        frame_cadence_ready, frame_wait, handle_client, record_event_render_request,
        render_clients, ClientSender, ClientState, EventRenderSource, FrameSendError, RenderCause,
        RenderRequest, RenderScratch, FRAME_INTERVAL,
    };
    use crate::app::App;
    use crate::event::{AppEvent, ClientInput};
    use crate::ipc::protocol::{FrameDiff, SurfaceInterest};
    use crate::terminal::appearance::PaneAppearance;
    use crate::terminal::vt::{create_engine, VtEngineKind};
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::Duration;

    /// A client built before `Hello` carried anything but these three fields must
    /// still receive the version-mismatch `Welcome`. If the server cannot decode
    /// the frame it hangs up instead, and the client reports an opaque IO error
    /// rather than telling the user to run `luvus server restart`.
    #[test]
    fn legacy_hello_still_gets_the_version_mismatch_reply() {
        #[derive(serde::Serialize)]
        enum LegacyClientMessage {
            Hello { version: u32, cols: u16, rows: u16 },
        }

        let dir = std::env::temp_dir().join(format!("luvus-legacy-hello-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("s");
        let _ = std::fs::remove_file(&sock);
        let listener = crate::ipc::transport::bind(&sock).unwrap();

        let server = std::thread::spawn(move || {
            let conn = crate::ipc::transport::incoming(&listener).next().unwrap();
            let (tx, _rx) = mpsc::channel();
            handle_client(1, conn, tx, Arc::new(AtomicBool::new(false)));
        });

        let mut client = crate::ipc::transport::connect(&sock).unwrap();
        // An older release announces protocol 6 with the frozen three-field shape.
        crate::ipc::protocol::write_message(
            &mut client,
            &LegacyClientMessage::Hello {
                version: 6,
                cols: 80,
                rows: 24,
            },
        )
        .unwrap();

        let mut reader = std::io::BufReader::new(client);
        let reply = crate::ipc::protocol::read_message::<_, ServerMessage>(&mut reader)
            .expect("the server must answer instead of hanging up");
        match reply {
            ServerMessage::Welcome { version, error } => {
                assert_eq!(version, crate::ipc::protocol::PROTOCOL_VERSION);
                assert_eq!(error.as_deref(), Some("protocol version mismatch"));
            }
            _ => panic!("expected a Welcome carrying the mismatch error"),
        }

        server.join().unwrap();
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_dir(&dir);
    }

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
                0,
                0,
                None,
                activity,
            ),
            rx,
        )
    }

    #[test]
    fn machine_catalog_changes_reach_only_machine_aware_clients() {
        let (mut machine_client, machine_rx) = display_client(80, 24, 2);
        machine_client.machine_capable = true;
        let (plain_client, plain_rx) = display_client(80, 24, 1);
        let mut clients = HashMap::from([(1, plain_client), (2, machine_client)]);

        broadcast_machine_catalog_changed(&mut clients, 7);

        assert!(matches!(
            machine_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            ServerMessage::MachineCatalogChanged { revision: 7 }
        ));
        assert!(plain_rx.try_recv().is_err());
        assert_eq!(clients.len(), 2);
    }

    fn received_frame_size(rx: &mpsc::Receiver<ServerMessage>) -> (u16, u16) {
        match rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            ServerMessage::Frame(frame) => (frame.width, frame.height),
            ServerMessage::FrameDiff(frame) => (frame.width, frame.height),
            _ => panic!("expected rendered frame"),
        }
    }

    #[test]
    fn prepared_frame_echoes_latest_ticket_without_taking_input_ownership() {
        let _env = crate::persist::test_env("prepared-frame-ticket");
        let (app_tx, _app_rx) = mpsc::channel();
        let mut app = App::new(80, 24, app_tx).unwrap();
        let (client, rx) = display_client(80, 24, 1);
        let mut clients = HashMap::from([(7, client)]);
        let mut foreground = Some(7);
        let mut interactive_size = (80, 24);
        let mut activity = 2;
        for ticket in [10, 11] {
            assert!(apply(
                AppEvent::ClientPrepareSurface {
                    id: 7,
                    ticket,
                    cols: 60,
                    rows: 20
                },
                &mut app,
                &mut clients,
                &mut foreground,
                &mut interactive_size,
                &mut activity,
            ));
        }
        assert_eq!(foreground, None);
        assert_eq!(clients[&7].interest, SurfaceInterest::Prepared);
        assert!(apply(
            AppEvent::ClientInput {
                id: 7,
                input: ClientInput::Resize {
                    cols: 60,
                    rows: 20,
                    cell_width_px: 9,
                    cell_height_px: 18
                },
            },
            &mut app,
            &mut clients,
            &mut foreground,
            &mut interactive_size,
            &mut activity,
        ));
        assert_eq!(
            (clients[&7].cell_width_px, clients[&7].cell_height_px),
            (9, 18)
        );
        assert_eq!(foreground, None, "candidate geometry cannot claim input");
        let mut scratch = RenderScratch::default();
        assert!(render_clients(
            &mut app,
            &mut clients,
            &mut foreground,
            &mut interactive_size,
            false,
            false,
            &mut scratch
        ));
        match rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            ServerMessage::PreparedFrame { ticket, frame } => {
                assert_eq!(ticket, 11);
                assert_eq!((frame.width, frame.height), (60, 20));
            }
            _ => panic!("expected ticketed candidate frame"),
        }
        assert!(clients[&7].prepare_ticket.is_none());
        assert_eq!(foreground, None);
    }

    #[test]
    fn suspended_surface_releases_frames_and_receives_no_render_or_effect() {
        let _env = crate::persist::test_env("suspended-surface");
        let (app_tx, _app_rx) = mpsc::channel();
        let mut app = App::new(80, 24, app_tx).unwrap();
        let (client, rx) = display_client(80, 24, 1);
        let mut clients = HashMap::from([(7, client)]);
        let mut foreground = Some(7);
        let mut interactive_size = (80, 24);
        let mut next_activity = 2;
        let mut scratch = RenderScratch::default();

        assert!(render_clients(
            &mut app,
            &mut clients,
            &mut foreground,
            &mut interactive_size,
            true,
            false,
            &mut scratch,
        ));
        assert!(matches!(rx.recv().unwrap(), ServerMessage::Frame(_)));
        clients[&7]
            .sender
            .frame_pending
            .store(false, Ordering::Release);

        assert!(apply(
            AppEvent::ClientSurfaceInterest {
                id: 7,
                interest: SurfaceInterest::Suspended,
            },
            &mut app,
            &mut clients,
            &mut foreground,
            &mut interactive_size,
            &mut next_activity,
        ));
        assert_eq!(foreground, None);
        assert!(clients[&7].last_frame.is_none());
        assert_eq!(
            clients[&7].render_buf.area,
            ratatui::layout::Rect::new(0, 0, 1, 1)
        );
        assert!(!render_clients(
            &mut app,
            &mut clients,
            &mut foreground,
            &mut interactive_size,
            true,
            false,
            &mut scratch,
        ));
        broadcast_effect(&mut clients, ServerMessage::Notify("hidden".into()));
        assert!(rx.try_recv().is_err());

        assert!(apply(
            AppEvent::ClientSurfaceInterest {
                id: 7,
                interest: SurfaceInterest::Prepared,
            },
            &mut app,
            &mut clients,
            &mut foreground,
            &mut interactive_size,
            &mut next_activity,
        ));
        assert!(render_clients(
            &mut app,
            &mut clients,
            &mut foreground,
            &mut interactive_size,
            true,
            false,
            &mut scratch,
        ));
        assert!(matches!(rx.recv().unwrap(), ServerMessage::Frame(_)));
    }

    #[test]
    fn client_sidebars_follow_native_files_input_without_changing_server_settings() {
        use super::render_client;
        use crate::ipc::protocol;
        let _env = crate::persist::test_env("machine-both-docks");
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(120, 30, tx).unwrap();
        let saved = app.sidebars.to_config();
        let saved_config = app.config.sidebars();
        let (mut client, _client_rx) = display_client(120, 30, 1);
        client.machine_capable = true;
        client.shell_dock_layout.owns_workspaces = true;
        let mut layout = saved.clone();
        layout.left.width = 34;
        layout.right.width = 30;
        layout.files_side = Some(crate::app::Side::Right);
        client.shell_sidebars = Some(protocol::ShellSidebars {
            revision: 5,
            layout,
            workspace_paths: true,
        });
        let mut clients = HashMap::from([(7, client)]);
        let mut foreground = Some(7);
        let mut size = (120, 30);
        let mut activity = 2;
        app.mode = crate::app::Mode::Prefix;
        apply(
            AppEvent::ClientInput {
                id: 7,
                input: ClientInput::Key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE)),
            },
            &mut app,
            &mut clients,
            &mut foreground,
            &mut size,
            &mut activity,
        );
        let updated = clients[&7].shell_sidebars.as_ref().unwrap().clone();
        assert_eq!(updated.revision, 6);
        assert!(updated.layout.right.visible);
        assert!(updated
            .layout
            .right
            .docks
            .iter()
            .any(|dock| dock == "files"));
        assert_eq!(updated.layout.left.width, 34);
        assert_eq!(updated.layout.right.width, 30);
        assert!(updated.workspace_paths);
        assert_eq!(app.sidebars.to_config(), saved);
        assert_eq!(app.config.sidebars(), saved_config);

        // A second endpoint starts with different server defaults but receives
        // the client's exact layout before rendering its own file content.
        let (mut remote, _remote_rx) = display_client(120, 30, 1);
        remote.machine_capable = true;
        remote.shell_dock_layout.owns_workspaces = true;
        remote.shell_sidebars = Some(updated);
        render_client(&mut app, &mut remote, true, true, false, &HashMap::new());
        assert_eq!(remote.last_shell_dock.unwrap().width, 34);
        assert!(
            remote
                .last_shell_sidebars
                .as_ref()
                .unwrap()
                .layout
                .right
                .visible
        );
        assert_eq!(app.sidebars.to_config(), saved);
        assert_eq!(app.config.sidebars(), saved_config);

        // An old broadcast cannot roll back newer native input from this client.
        assert!(!apply(
            AppEvent::ClientShellSidebars {
                id: 7,
                state: protocol::ShellSidebars {
                    revision: 5,
                    layout: saved.clone(),
                    workspace_paths: false,
                }
            },
            &mut app,
            &mut clients,
            &mut foreground,
            &mut size,
            &mut activity,
        ));
        assert_eq!(clients[&7].shell_sidebars.as_ref().unwrap().revision, 6);
    }

    #[test]
    fn client_sidebar_state_keeps_the_existing_frame_diff_baseline() {
        use crate::ipc::protocol;

        let _env = crate::persist::test_env("machine-sidebar-frame-diff");
        let (tx, _app_rx) = mpsc::channel();
        let mut app = App::new(120, 30, tx).unwrap();
        let (mut client, rx) = display_client(120, 30, 1);
        client.machine_capable = true;
        client.shell_dock_layout.owns_workspaces = true;
        let mut clients = HashMap::from([(7, client)]);
        let mut foreground = Some(7);
        let mut size = (120, 30);
        let mut activity = 2;
        let mut scratch = RenderScratch::default();

        assert!(render_clients(
            &mut app,
            &mut clients,
            &mut foreground,
            &mut size,
            true,
            false,
            &mut scratch,
        ));
        while !matches!(rx.recv().unwrap(), ServerMessage::Frame(_)) {}
        clients[&7]
            .sender
            .frame_pending
            .store(false, Ordering::Release);

        let mut layout = clients[&7].shell_sidebars.as_ref().unwrap().layout.clone();
        layout.left.width = 36;
        assert!(apply(
            AppEvent::ClientShellSidebars {
                id: 7,
                state: protocol::ShellSidebars {
                    revision: 1,
                    layout,
                    workspace_paths: true,
                },
            },
            &mut app,
            &mut clients,
            &mut foreground,
            &mut size,
            &mut activity,
        ));
        assert!(!clients[&7].force_full);
        assert!(render_clients(
            &mut app,
            &mut clients,
            &mut foreground,
            &mut size,
            false,
            false,
            &mut scratch,
        ));
        let mut rendered = None;
        for _ in 0..4 {
            match rx.recv_timeout(Duration::from_secs(1)).unwrap() {
                ServerMessage::Frame(_) => panic!("sidebar update sent an avoidable full frame"),
                ServerMessage::FrameDiff(diff) => {
                    rendered = Some(diff);
                    break;
                }
                _ => {}
            }
        }
        assert!(rendered.is_some(), "sidebar update produced a frame diff");
    }

    #[test]
    fn machine_workspace_path_toggle_is_client_owned_across_endpoints() {
        use crate::ipc::protocol;

        let _env = crate::persist::test_env("machine-workspace-path-owner");
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(120, 30, tx).unwrap();
        let persistent_workspace_paths = app.config.layout.workspace_paths;
        let (mut client, _client_rx) = display_client(120, 30, 1);
        client.machine_capable = true;
        client.shell_dock_layout.owns_workspaces = true;
        client.shell_sidebars = Some(protocol::ShellSidebars {
            revision: 5,
            layout: app.sidebars.to_config(),
            workspace_paths: true,
        });
        let mut clients = HashMap::from([(7, client)]);
        let mut foreground = Some(7);
        let mut size = (120, 30);
        let mut activity = 2;

        app.open_ws_menu(0, 10, 5);
        app.ws_menu.as_mut().unwrap().selected = Some(3);
        assert!(apply(
            AppEvent::ClientInput {
                id: 7,
                input: ClientInput::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            },
            &mut app,
            &mut clients,
            &mut foreground,
            &mut size,
            &mut activity,
        ));

        let state = clients[&7].shell_sidebars.as_ref().unwrap();
        assert_eq!(state.revision, 6);
        assert!(!state.workspace_paths);
        assert_eq!(
            app.config.layout.workspace_paths, persistent_workspace_paths,
            "the endpoint server config must not absorb client shell state"
        );
    }

    #[test]
    fn workspace_width_override_is_per_client_and_never_persisted() {
        use super::render_client;
        use crate::ipc::protocol::ShellDockLayout;
        let _env = crate::persist::test_env("machine-width-owner");
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(120, 30, tx).unwrap();
        let saved = (app.sidebars.left.width, app.sidebars.right.width);
        let (mut first, _first_rx) = display_client(120, 30, 1);
        let (mut second, _second_rx) = display_client(120, 30, 2);
        first.machine_capable = true;
        second.machine_capable = true;
        first.shell_dock_layout = ShellDockLayout {
            owns_workspaces: true,
            workspace_width: Some(34),
            ..ShellDockLayout::default()
        };
        second.shell_dock_layout = ShellDockLayout {
            owns_workspaces: true,
            workspace_width: Some(42),
            ..ShellDockLayout::default()
        };
        render_client(&mut app, &mut first, true, true, false, &HashMap::new());
        assert_eq!(first.last_shell_dock.unwrap().width, 34);
        assert_eq!(first.last_shell_dock.unwrap().resize.unwrap().column, 33);
        assert_eq!((app.sidebars.left.width, app.sidebars.right.width), saved);
        for width in [44, 26] {
            let state = first.shell_sidebars.as_mut().unwrap();
            state.layout.left.width = width;
            first.sidebar_cache = Some(crate::app::Sidebars::from_config(&state.layout));
            first.retained_ready = false;
            render_client(&mut app, &mut first, false, true, false, &HashMap::new());
            let dock = first.last_shell_dock.unwrap();
            assert_eq!(dock.width, width);
            let resize = dock.resize.expect("resized left dock retains its seam");
            assert!(resize.left);
            assert_eq!(resize.column, width - 1);
        }
        render_client(&mut app, &mut second, false, true, false, &HashMap::new());
        assert_eq!(second.last_shell_dock.unwrap().width, 42);
        assert_eq!(first.last_shell_dock.unwrap().width, 26);
        assert_eq!((app.sidebars.left.width, app.sidebars.right.width), saved);
    }

    #[test]
    fn machine_endpoint_rows_follow_workspace_paths_without_resizing_the_pty() {
        let _env = crate::persist::test_env("machine-dock-slot");
        let (app_tx, _app_rx) = mpsc::channel();
        let mut app = App::new(100, 30, app_tx).unwrap();
        let pane = app.layout().focus;
        let (client, rx) = display_client(100, 30, 1);
        let mut clients = HashMap::from([(7, client)]);
        let mut foreground = Some(7);
        let mut interactive_size = (100, 30);
        let mut next_activity = 2;
        let mut scratch = RenderScratch::default();

        assert!(render_clients(
            &mut app,
            &mut clients,
            &mut foreground,
            &mut interactive_size,
            true,
            false,
            &mut scratch,
        ));
        assert!(matches!(rx.recv().unwrap(), ServerMessage::Frame(_)));
        clients[&7]
            .sender
            .frame_pending
            .store(false, Ordering::Release);
        let pty_size = app.panes[&pane].size();

        assert!(apply(
            AppEvent::ClientShellDockLayout {
                id: 7,
                layout: crate::ipc::protocol::ShellDockLayout {
                    workspace_width: None,
                    owns_workspaces: true,
                    owns_session_chrome: false,
                    rows: 4,
                    leading: false,
                    indent_workspaces: false,
                },
            },
            &mut app,
            &mut clients,
            &mut foreground,
            &mut interactive_size,
            &mut next_activity,
        ));
        assert!(render_clients(
            &mut app,
            &mut clients,
            &mut foreground,
            &mut interactive_size,
            true,
            false,
            &mut scratch,
        ));
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            ServerMessage::ShellSidebars(_)
        ));
        let workspaces = match rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            ServerMessage::ShellWorkspaces(workspaces) => workspaces,
            _ => panic!("machine-aware clients receive the local workspace projection first"),
        };
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].index, 0);
        assert!(workspaces[0].active);
        let slot = match rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            ServerMessage::ShellDock(Some(slot)) => slot,
            _ => panic!("machine-aware clients receive dock geometry before their frame"),
        };
        assert_eq!(slot.height, 13);
        assert!(slot.show_paths);
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            ServerMessage::Frame(_)
        ));
        assert_eq!(app.panes[&pane].size(), pty_size);
    }

    #[test]
    fn empty_machine_catalog_still_enables_first_time_workspace_picker_tab() {
        let _env = crate::persist::test_env("machine-empty-catalog-capability");
        let (app_tx, _app_rx) = mpsc::channel();
        let mut app = App::new(80, 24, app_tx).unwrap();
        let (client, _rx) = display_client(80, 24, 1);
        let mut clients = HashMap::from([(7, client)]);
        let mut foreground = Some(7);
        let mut interactive_size = (80, 24);
        let mut next_activity = 2;

        assert!(apply(
            AppEvent::ClientShellDockLayout {
                id: 7,
                layout: crate::ipc::protocol::ShellDockLayout::default(),
            },
            &mut app,
            &mut clients,
            &mut foreground,
            &mut interactive_size,
            &mut next_activity,
        ));
        assert!(clients[&7].machine_capable);
        assert_eq!(clients[&7].shell_dock_layout.rows, 0);
    }

    #[test]
    fn machine_client_can_focus_local_workspace_while_surface_is_suspended() {
        let _env = crate::persist::test_env("machine-local-workspace-focus");
        let (app_tx, _app_rx) = mpsc::channel();
        let mut app = App::new(80, 24, app_tx).unwrap();
        let (client, _rx) = display_client(80, 24, 1);
        let mut clients = HashMap::from([(7, client)]);
        let mut foreground = Some(7);
        let mut interactive_size = (80, 24);
        let mut next_activity = 2;

        assert!(apply(
            AppEvent::ClientShellDockLayout {
                id: 7,
                layout: crate::ipc::protocol::ShellDockLayout::default(),
            },
            &mut app,
            &mut clients,
            &mut foreground,
            &mut interactive_size,
            &mut next_activity,
        ));
        assert!(apply(
            AppEvent::ClientSurfaceInterest {
                id: 7,
                interest: SurfaceInterest::Suspended,
            },
            &mut app,
            &mut clients,
            &mut foreground,
            &mut interactive_size,
            &mut next_activity,
        ));
        app.sidebar_focus = Some(crate::app::SidebarListFocus::Workspaces);
        assert!(apply(
            AppEvent::ClientShellWorkspaceFocus {
                id: 7,
                workspace_id: app.workspaces[0].id.clone()
            },
            &mut app,
            &mut clients,
            &mut foreground,
            &mut interactive_size,
            &mut next_activity,
        ));
        assert_eq!(app.active_ws, 0);
        assert_eq!(app.sidebar_focus, None);
        assert_eq!(
            foreground, None,
            "focus does not prematurely activate the surface"
        );
        assert!(apply(
            AppEvent::ClientOpenWorkspacePicker { id: 7 },
            &mut app,
            &mut clients,
            &mut foreground,
            &mut interactive_size,
            &mut next_activity,
        ));
        assert!(app.picker.is_some());
        assert!(!apply(
            AppEvent::ClientShellWorkspaceFocus {
                id: 7,
                workspace_id: "missing-workspace".into()
            },
            &mut app,
            &mut clients,
            &mut foreground,
            &mut interactive_size,
            &mut next_activity,
        ));
    }

    #[test]
    fn machine_workspace_menu_uses_native_identity_and_requires_active_surface() {
        let _env = crate::persist::test_env("machine-workspace-menu");
        let (app_tx, _app_rx) = mpsc::channel();
        let mut app = App::new(80, 24, app_tx).unwrap();
        let workspace_id = app.workspaces[0].id.clone();
        let (mut client, _rx) = display_client(80, 24, 1);
        client.machine_capable = true;
        client.interest = SurfaceInterest::Suspended;
        let mut clients = HashMap::from([(7, client)]);
        let mut foreground = Some(7);
        let mut size = (80, 24);
        let mut activity = 2;
        let menu = |workspace_id| AppEvent::ClientShellWorkspaceMenu {
            id: 7,
            workspace_id,
            col: 10,
            row: 5,
        };
        assert!(!apply(
            menu(workspace_id.clone()),
            &mut app,
            &mut clients,
            &mut foreground,
            &mut size,
            &mut activity
        ));
        assert!(app.ws_menu.is_none());
        clients.get_mut(&7).unwrap().interest = SurfaceInterest::Active;
        assert!(!apply(
            menu("stale-workspace".into()),
            &mut app,
            &mut clients,
            &mut foreground,
            &mut size,
            &mut activity
        ));
        assert!(app.ws_menu.is_none());
        assert!(apply(
            menu(workspace_id.clone()),
            &mut app,
            &mut clients,
            &mut foreground,
            &mut size,
            &mut activity
        ));
        assert_eq!(app.ws_menu.as_ref().unwrap().workspace_id, workspace_id);
        assert_eq!(app.ws_menu_target_index(), Some(0));
        assert_eq!(app.active_ws, 0);
    }

    #[test]
    fn hidden_agent_titles_present_once_including_coalesced_output() {
        let _env = crate::persist::test_env("hidden-agent-title");
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(100, 30, tx).unwrap();
        let hidden = app.layout().focus;
        app.status.get_mut(&hidden).unwrap().agent = "claude".into();
        let (tx, _rx) = mpsc::channel();
        let engine = create_engine(
            VtEngineKind::Alacritty,
            100,
            30,
            tx,
            4 * 1024 * 1024,
            PaneAppearance::default(),
        );
        app.panes.get_mut(&hidden).unwrap().engine = engine.clone();
        app.dispatch("tab.new", &serde_json::json!({})).unwrap();
        assert!(!app.pane_is_visible(hidden));
        app.config.layout.agent_title = true;

        engine.lock().unwrap().advance(b"\x1b]2;reviewing\x07");
        let source = super::event_render_source(&app, &AppEvent::PtyData(hidden));
        let mut request = RenderRequest::default();
        record_event_render_request(source, true, &mut request);
        assert!(request.needs_render());
        assert!(matches!(
            source,
            EventRenderSource::Cause(RenderCause::Metadata)
        ));
        engine
            .lock()
            .unwrap()
            .advance(b"ordinary output\x1b]2;reviewing\x07");
        assert!(matches!(
            super::event_render_source(&app, &AppEvent::PtyData(hidden)),
            EventRenderSource::HiddenPty
        ));

        // Output may arrive while the reader's notification is already set.
        engine.lock().unwrap().advance(b"\x1b]2;finished\x07");
        app.panes[&hidden].mark_data_pending_for_test();
        assert!(app.rearm_pty_notify_by_visibility().2);
        app.panes[&hidden].mark_data_pending_for_test();
        assert!(!app.rearm_pty_notify_by_visibility().2);
    }

    #[test]
    fn passive_retained_frames_match_full_projection_at_each_client_size() {
        use ratatui::{buffer::Buffer, layout::Rect};
        let _env = crate::persist::test_env("passive-retained-frames");
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(100, 30, tx).unwrap();
        app.server_mode = true;
        let pane = app.layout().focus;
        let (tx, _rx) = mpsc::channel();
        let engine = create_engine(
            VtEngineKind::Alacritty,
            100,
            30,
            tx,
            4 * 1024 * 1024,
            PaneAppearance::default(),
        );
        app.panes.get_mut(&pane).unwrap().engine = engine.clone();
        let mut clients = HashMap::new();
        let mut receivers = Vec::new();
        for (id, cols, rows) in [(1, 100, 30), (2, 100, 30), (3, 60, 20)] {
            let (client, rx) = display_client(cols, rows, id);
            clients.insert(id, client);
            receivers.push(rx);
        }
        let mut foreground = Some(1);
        let mut size = (100, 30);
        let mut scratch = RenderScratch::default();
        render_clients(
            &mut app,
            &mut clients,
            &mut foreground,
            &mut size,
            false,
            false,
            &mut scratch,
        );
        for rx in &receivers {
            rx.recv_timeout(Duration::from_secs(1)).unwrap();
        }
        let geometry = app.pane_content_rects.clone();
        let pty_size = app.panes[&pane].size();

        for bytes in [
            "界e\u{301} text",
            "\rshort\x1b[K",
            "\x1b]2;new title\x07!",
            "\rnext",
        ] {
            for client in clients.values() {
                client.sender.frame_pending.store(false, Ordering::Release);
            }
            engine.lock().unwrap().advance(bytes.as_bytes());
            let partial_before = super::PARTIAL_TERMINAL_PROJECTIONS.load(Ordering::Relaxed);
            render_clients(
                &mut app,
                &mut clients,
                &mut foreground,
                &mut size,
                false,
                true,
                &mut scratch,
            );
            for rx in &receivers {
                rx.recv_timeout(Duration::from_secs(1)).unwrap();
            }
            if !bytes.contains("\x1b]2;") {
                assert!(
                    super::PARTIAL_TERMINAL_PROJECTIONS.load(Ordering::Relaxed)
                        >= partial_before + 3,
                    "all three clients must patch their own retained geometry"
                );
            }
            for client in clients.values() {
                let area = Rect::new(0, 0, client.size.0, client.size.1);
                let mut expected = Buffer::empty(area);
                let mut target = crate::ui::RenderTarget::new(&mut expected, area);
                crate::ui::render_projection(&mut target, &mut app);
                let cursor = (target.cursor(), target.cursor_visible());
                assert_eq!(client.render_buf, expected, "client size {:?}", client.size);
                let frame = client.last_frame.as_ref().unwrap();
                assert_eq!((frame.cursor, frame.cursor_visible), cursor);
                assert!(client.retained_ready);
            }
            assert_eq!(app.pane_content_rects, geometry);
            assert_eq!(app.panes[&pane].size(), pty_size);
            assert_eq!(app.layout().focus, pane);
        }
    }

    #[test]
    fn visible_pty_only_frame_uses_retained_rows_after_full_baseline() {
        let _env = crate::persist::test_env("server-retained-terminal-rows");
        let (app_tx, _app_rx) = mpsc::channel();
        let mut app = App::new(100, 30, app_tx).expect("app starts");
        app.server_mode = true;
        app.config.layout.agent_title = true;
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
            0,
            0,
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

    #[test]
    fn image_staged_for_a_removed_client_is_discarded() {
        let _env = crate::persist::test_env("removed-client-image");
        let (app_tx, _app_rx) = mpsc::channel();
        let mut app = App::new(80, 24, app_tx).expect("app starts");
        let png = crate::clipboard_image::encode_rgba_png(1, 1, |_, _| [1, 2, 3, 255])
            .expect("fixture PNG");
        let staged = crate::clipboard_image::stage_png(&png).expect("staged image");
        let mut clients = HashMap::new();
        let mut foreground = None;
        let mut interactive_size = (80, 24);
        let mut next_activity = 1;

        assert!(!apply(
            AppEvent::ClientInput {
                id: 7,
                input: ClientInput::PasteImage(staged.clone()),
            },
            &mut app,
            &mut clients,
            &mut foreground,
            &mut interactive_size,
            &mut next_activity,
        ));
        assert!(!staged.exists());
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
    fn machine_selector_does_not_end_the_client_writer() {
        assert!(!ends_client_writer(&ServerMessage::OpenMachineSelector));
        assert!(!ends_client_writer(&ServerMessage::OpenMachineCreate {
            theme: crate::ipc::protocol::MachineFormTheme {
                surface: 0,
                border: 0,
                text: 0,
                subtext0: 0,
                subtext1: 0,
                accent: 0,
                accent_text: 0,
                divider: 0,
                rule: 0,
                error: 0,
            },
            modal: crate::ipc::protocol::ShellDockBlock {
                x: 2,
                y: 2,
                width: 40,
                height: 20,
            },
        }));
        assert!(ends_client_writer(&ServerMessage::Detach));
        assert!(!ends_client_writer(&ServerMessage::SwitchSession {
            name: "review".into(),
        }));
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
                input: ClientInput::Resize {
                    cols: 46,
                    rows: 16,
                    cell_width_px: 0,
                    cell_height_px: 0,
                },
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

    fn received_open_path(rx: &mpsc::Receiver<ServerMessage>) -> Option<String> {
        while let Ok(msg) = rx.try_recv() {
            if let ServerMessage::OpenPath(path) = msg {
                return Some(path);
            }
        }
        None
    }

    /// Open Externally is a desktop effect, so it goes to the one client whose
    /// input queued it. The other attached display must not receive it, even
    /// though it is the foreground client when the input arrives.
    #[test]
    fn open_path_is_routed_to_the_initiating_client_only() {
        assert_open_path_routes_to_initiator(false);
    }

    #[test]
    fn open_path_is_routed_after_machine_sidebar_input() {
        assert_open_path_routes_to_initiator(true);
    }

    fn assert_open_path_routes_to_initiator(machine_capable: bool) {
        let _env = crate::persist::test_env("multi-client-open-path");
        let (app_tx, _app_rx) = mpsc::channel();
        let mut app = App::new(120, 40, app_tx).expect("app starts");
        app.server_mode = true;
        let (large, large_rx) = display_client(120, 40, 2);
        let (mut small, small_rx) = display_client(50, 20, 1);
        small.machine_capable = machine_capable;
        small.shell_dock_layout.owns_workspaces = machine_capable;
        let mut clients = HashMap::from([(1, large), (2, small)]);
        let mut foreground = Some(1);
        let mut interactive_size = (120, 40);
        let mut next_activity = 3;

        // Stand in for the FILES action the key would trigger: the path is
        // already queued when client 2's input is applied.
        let path = "/tmp/notes.pdf".to_string();
        app.pending_open_path = Some(path.clone());
        apply(
            AppEvent::ClientInput {
                id: 2,
                input: ClientInput::Key(KeyEvent::new(KeyCode::Null, KeyModifiers::NONE)),
            },
            &mut app,
            &mut clients,
            &mut foreground,
            &mut interactive_size,
            &mut next_activity,
        );

        assert!(app.pending_open_path.is_none(), "drained by the input path");
        assert_eq!(
            received_open_path(&small_rx).as_ref(),
            Some(&path),
            "the initiating client receives the path"
        );
        assert!(
            received_open_path(&large_rx).is_none(),
            "the other attached client must not open it too"
        );
    }
}
