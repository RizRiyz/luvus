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
/// Background-only PTY output cannot change the active terminal grid. A 10fps
/// probe is enough for sidebar state while avoiding full UI render/diff work
/// for every inactive-pane read. Any visible output or interaction immediately
/// returns to [`FRAME_INTERVAL`].
const BACKGROUND_FRAME_INTERVAL: Duration = Duration::from_millis(100);
/// How often to wake when idle (drives agent detection + toast expiry) — coarser
/// than the frame cap so an idle session doesn't spin the CPU.
const IDLE_INTERVAL: Duration = Duration::from_millis(33);

static FRAMES_SENT: AtomicU64 = AtomicU64::new(0);
static FULL_FRAMES_SENT: AtomicU64 = AtomicU64::new(0);
static DIFF_RUNS_SENT: AtomicU64 = AtomicU64::new(0);
static FRAME_BYTES_SENT: AtomicU64 = AtomicU64::new(0);
static RENDER_PASSES: AtomicU64 = AtomicU64::new(0);

/// Process-lifetime frame counters for performance diagnostics.
pub fn performance_snapshot() -> serde_json::Value {
    serde_json::json!({
        "frames_sent": FRAMES_SENT.load(Ordering::Relaxed),
        "full_frames_sent": FULL_FRAMES_SENT.load(Ordering::Relaxed),
        "diff_runs_sent": DIFF_RUNS_SENT.load(Ordering::Relaxed),
        "frame_bytes_sent": FRAME_BYTES_SENT.load(Ordering::Relaxed),
        "render_passes": RENDER_PASSES.load(Ordering::Relaxed),
    })
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
    last_activity: u64,
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
    if transport::connect(&sock).is_ok() || transport::connect(&client_sock).is_ok() {
        return Ok(());
    }
    api::set_socket_path(sock.clone());

    let events = api::new_bus();
    let api_listener = api::bind_server(&sock, &startup_lock)?;
    let client_listener = match bind_client_listener(&client_sock, &startup_lock) {
        Ok(listener) => listener,
        Err(err) => {
            drop(api_listener);
            let _ = remove_unbound_socket(&sock);
            return Err(err.into());
        }
    };

    let mut app = match App::restore_or_new(DEFAULT_SIZE.0, DEFAULT_SIZE.1, tx.clone()) {
        Ok(app) => app,
        Err(err) => {
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
    api::start_server_with_uhp(
        api_listener,
        tx.clone(),
        events,
        Arc::clone(&app.uhp_available),
    );
    start_client_listener(client_listener, tx.clone(), terminal_theme.clone());
    drop(startup_lock);
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
    // Geometry last committed to the shared PTYs and interactive hit-test state.
    // Secondary-client projections never change it.
    let mut interactive_size = DEFAULT_SIZE;
    let mut next_activity = 1u64;
    let mut last_draw = Instant::now();
    let mut last_save = Instant::now();
    // Un-rendered activity waiting for the frame cap to expire — drives a trailing
    // render so a change that lands mid-interval isn't stuck until the next event.
    let mut dirty = false;
    // True only while every pending redraw reason is output from a pane outside
    // the active tab. Visible output and UI state changes always promote the
    // pending frame back to the normal interactive cadence.
    let mut background_only = false;
    // Advances the working-agent spinner ~10x/s (the idle tick already wakes the
    // loop every IDLE_INTERVAL, so this just gates the frame + a repaint).
    let mut last_spin = Instant::now();
    const SPIN_INTERVAL: Duration = Duration::from_millis(100);
    // Fallback re-arm cadence for PTY wake coalescing when frames aren't being
    // rendered (no client attached / nothing dirty): readers may announce new
    // output ~10x/s. While rendering, the render path re-arms at the frame rate.
    let mut last_rearm = Instant::now();
    const REARM_INTERVAL: Duration = Duration::from_millis(100);

    loop {
        // Pending + clients attached → wait only until the cap frees up (flush
        // promptly); otherwise tick at the coarser idle cadence.
        let current_interval = frame_interval(background_only);
        let wait = if dirty && !clients.is_empty() {
            current_interval
                .saturating_sub(last_draw.elapsed())
                .max(Duration::from_millis(1))
        } else {
            IDLE_INTERVAL
        };
        let mut foreground_activity = false;
        let mut background_activity = false;
        let mut activity = match rx.recv_timeout(wait) {
            Ok(ev) => {
                let background = background_pty_event(&app, &ev);
                let changed = apply(
                    ev,
                    &mut app,
                    &mut clients,
                    &mut foreground,
                    &mut interactive_size,
                    &mut next_activity,
                );
                foreground_activity = changed && !background;
                background_activity = changed && background;
                changed
            }
            Err(RecvTimeoutError::Timeout) => false,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        while let Ok(ev) = rx.try_recv() {
            let background = background_pty_event(&app, &ev);
            let changed = apply(
                ev,
                &mut app,
                &mut clients,
                &mut foreground,
                &mut interactive_size,
                &mut next_activity,
            );
            activity |= changed;
            foreground_activity |= changed && !background;
            background_activity |= changed && background;
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
        // The last node closed (docs/43 §3.3): the *session* is over, so every
        // window goes away — not just the foreground one, which would leave other
        // clients staring at a session with nothing in it. The server stays up
        // with no nodes; `server stop` is still what ends it.
        if app.end_session {
            app.end_session = false;
            // Detach is a reliable control message. It does not compete with the
            // one-frame backpressure slot, so a busy or remote client cannot
            // lose it and cannot block this loop.
            for (_, client) in clients.drain() {
                let _ = client.send_control(ServerMessage::Detach);
            }
            foreground = None;
            // Persist immediately rather than waiting out the 2s debounce: the
            // snapshot is now empty, which *removes* `session.json`, and a kill
            // inside that window would otherwise leave the closed nodes on disk
            // to be restored on the next start.
            persist::save(&app);
            app.session_dirty = false;
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
                activity = true;
                foreground_activity = true;
            }
        }
        if let Some(name) = app.pending_session_switch.take() {
            if let Some(id) = foreground.take() {
                if let Some(client) = clients.remove(&id) {
                    let _ = client.send_control(ServerMessage::SwitchSession { name });
                }
                foreground = latest_client(&clients);
                apply_foreground_theme(&mut app, &clients, foreground);
                activity = true;
                foreground_activity = true;
            } else {
                app.show_toast("no attached client to switch".to_string());
            }
        }

        if app.session_dirty && last_save.elapsed() > Duration::from_secs(2) {
            persist::save(&app);
            app.session_dirty = false;
            last_save = Instant::now();
        }

        // A state transition here (e.g. a silent agent reaching Done) has no PtyData
        // to ride on, so repaint when detection reports a visible change.
        let now = Instant::now();
        if app.detect_tick(now) {
            activity = true;
            foreground_activity = true;
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
        if app.pending_sound {
            app.pending_sound = false;
            broadcast(&mut clients, ServerMessage::Sound);
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
            activity = true;
            foreground_activity = true;
        }
        // Likewise for an expired search-jump flash (docs/63).
        if app.tick_search_flash(Instant::now()) {
            activity = true;
            foreground_activity = true;
        }
        if app.tick_bar_notifications(now) {
            activity = true;
            foreground_activity = true;
        }
        // Animate the sidebar spinner while any agent is working: advance the
        // frame and mark dirty so the diff sends only the changed dot cell.
        if last_spin.elapsed() >= SPIN_INTERVAL
            && (app.any_working() || app.bar.has_visible_working(&app.config.bars, app.compact))
        {
            app.spinner = app.spinner.wrapping_add(1);
            last_spin = Instant::now();
            dirty = true;
            background_only = false;
        }
        if activity {
            background_only = next_background_only(
                dirty,
                background_only,
                foreground_activity,
                background_activity,
            );
            dirty = true;
        }
        // Fallback re-arm (the render path below re-arms at the frame rate): a
        // flag still set here means un-rendered output → schedule a frame.
        if last_rearm.elapsed() >= REARM_INTERVAL {
            last_rearm = Instant::now();
            let (visible, background) = app.rearm_pty_notify_by_visibility();
            if visible {
                dirty = true;
                background_only = false;
            } else if background {
                background_only = background_only || !dirty;
                dirty = true;
            }
        }

        // A forced redraw (resize / focus-regained / external damage) must render
        // even if nothing else changed this tick — and so must a client that is
        // waiting on its full-frame resync (see `needs_render`).
        let any_behind = clients.values().any(|client| client.behind);
        let urgent = app.force_redraw || any_behind;
        dirty = needs_render(dirty, app.force_redraw, any_behind);
        if urgent {
            background_only = false;
        }

        if dirty && !clients.is_empty() && last_draw.elapsed() >= frame_interval(background_only) {
            let forced = std::mem::take(&mut app.force_redraw);
            render_clients(
                &mut app,
                &mut clients,
                &mut foreground,
                &mut interactive_size,
                forced,
            );
            last_draw = Instant::now();
            // Re-arm the PTY readers now that their output is on screen. A flag
            // set during this frame = more output already waiting → stay dirty
            // so the burst keeps rendering at the frame cap, tail included.
            let (visible, background) = app.rearm_pty_notify_by_visibility();
            dirty = visible || background;
            background_only = background && !visible;
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
                let disconnected = clients
                    .get_mut(&id)
                    .is_some_and(|client| render_client(app, client, true, false));
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

fn background_pty_event(app: &App, event: &AppEvent) -> bool {
    matches!(event, AppEvent::PtyData(id) if !app.pane_is_visible(*id))
}

fn frame_interval(background_only: bool) -> Duration {
    if background_only {
        BACKGROUND_FRAME_INTERVAL
    } else {
        FRAME_INTERVAL
    }
}

fn next_background_only(
    dirty: bool,
    background_only: bool,
    foreground_activity: bool,
    background_activity: bool,
) -> bool {
    if foreground_activity {
        false
    } else if !dirty {
        background_activity
    } else {
        background_only
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
fn needs_render(app_dirty: bool, force_redraw: bool, any_behind: bool) -> bool {
    app_dirty || force_redraw || any_behind
}

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
) {
    RENDER_PASSES.fetch_add(1, Ordering::Relaxed);
    if foreground.is_none_or(|id| !clients.contains_key(&id)) {
        *foreground = latest_client(clients);
        apply_foreground_theme(app, clients, *foreground);
    }

    let mut order: Vec<u64> = clients.keys().copied().collect();
    order.sort_unstable_by_key(|id| (*foreground != Some(*id), *id));
    let mut dead = Vec::new();
    for id in order {
        let interactive = *foreground == Some(id);
        if let Some(client) = clients.get_mut(&id) {
            if render_client(app, client, interactive, force_all) {
                dead.push(id);
            } else if interactive {
                *interactive_size = client.size;
            }
        }
    }
    for id in dead {
        clients.remove(&id);
    }
    if foreground.is_some_and(|id| !clients.contains_key(&id)) {
        *foreground = latest_client(clients);
        apply_foreground_theme(app, clients, *foreground);
    }
}

/// Render and enqueue one client's next frame. Returns true when its writer is
/// disconnected and the caller should remove it.
fn render_client(
    app: &mut App,
    client: &mut ClientState,
    interactive: bool,
    force_all: bool,
) -> bool {
    let area = Rect::new(0, 0, client.size.0, client.size.1);
    if client.render_buf.area != area {
        client.render_buf = Buffer::empty(area);
        client.last_frame = None;
        client.force_full = true;
    } else {
        client.render_buf.reset();
    }

    let cursor = {
        let mut target = ui::RenderTarget::new(&mut client.render_buf, area);
        if interactive {
            ui::render_into(&mut target, app);
        } else {
            ui::render_projection(&mut target, app);
        }
        target.cursor()
    };

    let full = force_all
        || client.force_full
        || client.behind
        || client.last_frame.as_ref().is_none_or(|previous| {
            previous.width != client.render_buf.area.width
                || previous.height != client.render_buf.area.height
        });
    let message = if full {
        client.last_frame = Some(protocol::frame_from_buffer(&client.render_buf, cursor));
        Some(ServerMessage::Frame(
            client.last_frame.as_ref().expect("frame stored").clone(),
        ))
    } else {
        let previous = client.last_frame.as_mut().expect("frame baseline exists");
        let cursor_moved = previous.cursor != cursor;
        let runs = protocol::diff_buffer(previous, &client.render_buf);
        previous.cursor = cursor;
        if runs.is_empty() && !cursor_moved {
            None
        } else {
            Some(ServerMessage::FrameDiff(protocol::FrameDiff {
                width: previous.width,
                height: previous.height,
                runs,
                cursor,
            }))
        }
    };

    let Some(message) = message else {
        return false;
    };
    match client.sender.try_send_frame(message) {
        Ok(()) => {
            client.behind = false;
            client.force_full = false;
            false
        }
        Err(FrameSendError::Full) => {
            client.behind = true;
            false
        }
        Err(FrameSendError::Disconnected) => true,
    }
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
/// idle tick (≤33ms) and exits through the normal path — clients notified, the
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
        apply, broadcast, frame_interval, needs_render, next_background_only, render_clients,
        ClientSender, ClientState, FrameSendError, BACKGROUND_FRAME_INTERVAL, FRAME_INTERVAL,
    };
    use crate::app::App;
    use crate::event::{AppEvent, ClientInput};
    use crate::ipc::protocol::FrameDiff;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;
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

    /// A client that dropped a diff must get its full-frame resync even when the
    /// screen goes quiet. The resync only ships from inside a render, so a
    /// pending `behind` entry has to count as work — otherwise a client that fell
    /// behind just as a burst of agent output ended would keep showing stale
    /// cells until something unrelated redrew the screen.
    #[test]
    fn a_behind_client_forces_a_frame_on_an_idle_screen() {
        assert!(
            needs_render(false, false, true),
            "a pending resync renders even with nothing else to do"
        );
        // The pre-existing reasons still hold.
        assert!(needs_render(true, false, false), "app activity renders");
        assert!(needs_render(false, true, false), "a forced redraw renders");
        // And a genuinely idle loop with every client up to date stays idle, so
        // this cannot spin the render loop on a quiet screen.
        assert!(
            !needs_render(false, false, false),
            "nothing to do means no frame"
        );
    }

    #[test]
    fn background_frames_use_the_slower_cap_only_while_background_only() {
        assert_eq!(frame_interval(false), FRAME_INTERVAL);
        assert_eq!(frame_interval(true), BACKGROUND_FRAME_INTERVAL);
        assert!(BACKGROUND_FRAME_INTERVAL > FRAME_INTERVAL);

        assert!(next_background_only(false, false, false, true));
        assert!(next_background_only(true, true, false, true));
        assert!(!next_background_only(true, true, true, true));
        assert!(!next_background_only(true, false, false, true));
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

    /// Session detach is carried by the same reliable control path. Preserve the
    /// earlier guarantee that closing the last node cannot strand a client just
    /// because its writer already has a frame waiting.
    #[test]
    fn ending_a_session_delivers_detach_behind_a_queued_frame() {
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

        render_clients(
            &mut app,
            &mut clients,
            &mut foreground,
            &mut interactive_size,
            false,
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
