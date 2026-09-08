//! Thin-client machine shell.
//!
//! This path is activated only when at least one saved profile exists. The
//! selected Luvus server still renders the product UI; the shell owns only the
//! compact machine dock, persistent SSH links, and generation-fenced surface
//! switching.

use std::collections::HashMap;
use std::io::BufReader;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use ratatui::buffer::Cell;
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, Event, KeyCode, MouseButton, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use ratatui::DefaultTerminal;

use crate::ipc::protocol::{
    self, ClientMessage, ServerMessage, ShellDockRect, SurfaceInterest, PROTOCOL_VERSION,
};
use crate::machine::catalog::MachineProfile;
use crate::machine::link::{self, LinkControl, LinkEvent, LinkTask};
use crate::machine::protocol as fleet;

const LINK_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const SURFACE_PREPARE_TIMEOUT: Duration = Duration::from_secs(15);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

enum ShellEvent {
    Input(ClientMessage),
    Local(ServerMessage),
    Link(LinkEvent),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Endpoint {
    Local,
    Remote {
        machine_id: String,
        channel_id: u64,
        session: String,
    },
}

enum MachineState {
    Disabled,
    Connecting { deadline: Instant },
    Online,
    Reconnecting { at: Instant },
    Attention(String),
}

struct MachineRuntime {
    profile: MachineProfile,
    state: MachineState,
    generation: u64,
    control: Option<LinkControl>,
    reader: Option<JoinHandle<()>>,
    backoff: Duration,
    sessions: Vec<fleet::SessionSummary>,
}

impl MachineRuntime {
    fn status(&self) -> &str {
        match &self.state {
            MachineState::Disabled => "disabled",
            MachineState::Connecting { .. } => "connecting",
            MachineState::Online => "online",
            MachineState::Reconnecting { .. } => "reconnecting",
            MachineState::Attention(reason) => reason,
        }
    }

    fn session(&self) -> String {
        self.profile
            .preferred_session
            .clone()
            .or_else(|| {
                self.sessions
                    .iter()
                    .find(|session| session.default)
                    .map(|session| session.name.clone())
            })
            .unwrap_or_else(|| crate::session::DEFAULT_SESSION_NAME.to_string())
    }
}

struct SurfaceCandidate {
    endpoint: Endpoint,
    generation: u64,
    welcomed: bool,
    ready: bool,
    shell_dock: Option<Option<ShellDockRect>>,
    deadline: Instant,
}

struct DockState {
    rect: Option<ShellDockRect>,
    selector_rect: Option<Rect>,
    selector_open: bool,
    selector_cursor: usize,
    refresh_catalog: bool,
    warning: Option<String>,
    heading: &'static str,
    close_label: &'static str,
    base: Cell,
    hits: Vec<(Endpoint, Rect)>,
    dirty: bool,
}

impl Default for DockState {
    fn default() -> Self {
        Self {
            rect: None,
            selector_rect: None,
            selector_open: false,
            selector_cursor: 0,
            refresh_catalog: false,
            warning: None,
            heading: crate::i18n::EN.machines,
            close_label: crate::i18n::EN.act_close,
            base: Cell::default(),
            hits: Vec::new(),
            dirty: true,
        }
    }
}

pub(super) fn run(
    reader: crate::ipc::transport::Conn,
    writer: crate::ipc::transport::Conn,
    profiles: Vec<MachineProfile>,
) -> Result<()> {
    let _logging = crate::logging::init(crate::logging::Role::Client);
    let mut terminal = ratatui::init();
    crate::install_tui_panic_hook();
    let result = run_inner(reader, writer, profiles, &mut terminal);
    let _ = execute!(
        std::io::stdout(),
        crossterm::event::PopKeyboardEnhancementFlags,
        DisableFocusChange,
        DisableMouseCapture,
        DisableBracketedPaste
    );
    ratatui::restore();
    match result? {
        super::client::ClientExit::Done => Ok(()),
        super::client::ClientExit::Detached => {
            crate::print_detached_status(crate::i18n::cli::Context::configured());
            Ok(())
        }
        super::client::ClientExit::ServerStopped => {
            let context = crate::i18n::cli::Context::configured();
            let session = crate::session::display_name();
            let rows = [
                (context.text("status"), context.text("stopped")),
                (context.text("session"), session.as_str()),
            ];
            crate::cli::print_status_card("Luvus session", &rows);
            Ok(())
        }
        super::client::ClientExit::SwitchSession(name) => {
            super::client::switch_session_process(&name)
        }
    }
}

fn run_inner(
    reader: crate::ipc::transport::Conn,
    mut writer: crate::ipc::transport::Conn,
    profiles: Vec<MachineProfile>,
    terminal: &mut DefaultTerminal,
) -> Result<super::client::ClientExit> {
    let truecolor = protocol::truecolor_supported();
    let size = terminal.size()?;
    protocol::write_message(
        &mut writer,
        &ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            cols: size.width,
            rows: size.height,
        },
    )?;
    let mut reader = BufReader::new(reader);
    match protocol::read_message::<_, ServerMessage>(&mut reader)? {
        ServerMessage::Welcome { error: None, .. } => {}
        ServerMessage::Welcome {
            error: Some(error), ..
        } => return Err(anyhow!(error)),
        _ => return Err(anyhow!("unexpected local server handshake")),
    }
    let probe_terminal = match protocol::read_message::<_, ServerMessage>(&mut reader)? {
        ServerMessage::Ready { probe_terminal } => probe_terminal,
        _ => return Err(anyhow!("unexpected local server negotiation")),
    };
    let probe = if probe_terminal {
        crate::terminal::theme_probe::probe()
    } else {
        crate::terminal::theme_probe::ProbeResult {
            colors: None,
            pending: Vec::new(),
        }
    };
    if probe_terminal {
        protocol::write_message(
            &mut writer,
            &ClientMessage::TerminalColors(probe.colors.clone()),
        )?;
    }

    let mut dock_rows = dock_row_count(profiles.len());
    protocol::write_message(&mut writer, &ClientMessage::ShellDockRows(dock_rows))?;
    protocol::write_message(
        &mut writer,
        &ClientMessage::SurfaceInterest(SurfaceInterest::Active),
    )?;

    let writer = Arc::new(Mutex::new(writer));
    let (events_tx, events_rx) = mpsc::channel();
    start_local_reader(reader, events_tx.clone())?;
    start_input_reader(probe.pending, events_tx.clone())?;

    let _ = execute!(
        std::io::stdout(),
        EnableBracketedPaste,
        EnableMouseCapture,
        EnableFocusChange,
        crossterm::terminal::SetTitle(crate::window_title())
    );
    #[cfg(windows)]
    let _windows_input_mode = crate::terminal::host_input::enable_input_mode();
    crate::push_key_protocol();

    let mut machines = profiles
        .into_iter()
        .map(|profile| {
            let state = if profile.enabled {
                MachineState::Reconnecting { at: Instant::now() }
            } else {
                MachineState::Disabled
            };
            (
                profile.id.clone(),
                MachineRuntime {
                    profile,
                    state,
                    generation: 0,
                    control: None,
                    reader: None,
                    backoff: Duration::from_secs(1),
                    sessions: Vec::new(),
                },
            )
        })
        .collect::<HashMap<_, _>>();
    let mut active = Endpoint::Local;
    let mut warm: Option<Endpoint> = None;
    let mut candidate: Option<SurfaceCandidate> = None;
    let mut next_channel = 1u64;
    let mut next_request = 1u64;
    let labels = crate::i18n::by_code(&crate::config::load().language);
    let mut dock = DockState {
        heading: labels.machines,
        close_label: labels.act_close,
        ..DockState::default()
    };
    let mut last_cursor = None;
    let mut cursor_visible = false;
    let mut exit = super::client::ClientExit::Done;
    let mut running = true;

    while running {
        start_due_links(&mut machines, &events_tx);
        expire_connecting(&mut machines);
        if expire_candidate(&mut candidate, &machines) {
            dock.warning = Some("surface preparation timed out".to_string());
            dock.dirty = true;
        }
        let timeout = next_deadline(&machines, candidate.as_ref())
            .map(|deadline| deadline.saturating_duration_since(Instant::now()));
        let event = match timeout {
            Some(timeout) => match events_rx.recv_timeout(timeout) {
                Ok(event) => Some(event),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            },
            None => match events_rx.recv() {
                Ok(event) => Some(event),
                Err(_) => break,
            },
        };
        let Some(event) = event else {
            continue;
        };
        match event {
            ShellEvent::Input(message) => {
                if handle_dock_input(
                    &message,
                    &mut dock,
                    &active,
                    &mut warm,
                    &mut candidate,
                    &mut next_channel,
                    &machines,
                    &writer,
                    terminal,
                )? {
                    continue;
                }
                if matches!(message, ClientMessage::Detach) {
                    exit = super::client::ClientExit::Detached;
                    running = false;
                    continue;
                }
                if let Err(error) = send_surface(&active, &message, &machines, &writer) {
                    if candidate.is_none() {
                        return Err(error);
                    }
                }
            }
            ShellEvent::Local(message) => {
                if let Some(reason) = handle_surface_message(
                    Endpoint::Local,
                    message,
                    &mut active,
                    &mut warm,
                    &mut candidate,
                    &machines,
                    &writer,
                    terminal,
                    truecolor,
                    &mut dock,
                    &mut last_cursor,
                    &mut cursor_visible,
                    dock_rows,
                    &mut next_channel,
                )? {
                    exit = reason;
                    running = false;
                }
            }
            ShellEvent::Link(event) => {
                if let Some(reason) = handle_link_event(
                    event,
                    &mut machines,
                    &mut active,
                    &mut warm,
                    &mut candidate,
                    &mut next_request,
                    &writer,
                    terminal,
                    truecolor,
                    &mut dock,
                    &mut last_cursor,
                    &mut cursor_visible,
                    dock_rows,
                    &mut next_channel,
                )? {
                    exit = reason;
                    running = false;
                }
            }
        }
        if dock.refresh_catalog {
            dock.refresh_catalog = false;
            refresh_catalog(
                &mut machines,
                &mut active,
                &mut warm,
                &mut candidate,
                &mut next_channel,
                &writer,
                &mut dock,
                &mut dock_rows,
            )?;
        }
        if dock.dirty {
            paint_dock(
                terminal,
                &mut dock,
                &machines,
                &active,
                &mut last_cursor,
                cursor_visible,
            )?;
        }
    }

    for machine in machines.values_mut() {
        stop_link(machine);
    }
    Ok(exit)
}

fn dock_row_count(machine_count: usize) -> u16 {
    if machine_count == 0 {
        return 0;
    }
    u16::try_from(machine_count.saturating_mul(2).saturating_add(2))
        .unwrap_or(12)
        .min(12)
}

fn start_local_reader(
    mut reader: BufReader<crate::ipc::transport::Conn>,
    events: Sender<ShellEvent>,
) -> Result<()> {
    std::thread::Builder::new()
        .name("client-surface".to_string())
        .stack_size(256 * 1024)
        .spawn(move || {
            while let Ok(message) = protocol::read_message::<_, ServerMessage>(&mut reader) {
                if events.send(ShellEvent::Local(message)).is_err() {
                    break;
                }
            }
        })?;
    Ok(())
}

fn start_input_reader(pending: Vec<Event>, events: Sender<ShellEvent>) -> Result<()> {
    std::thread::Builder::new()
        .name("client-input".to_string())
        .stack_size(256 * 1024)
        .spawn(move || {
            let send = |event| {
                super::client::event_message(event)
                    .is_none_or(|message| events.send(ShellEvent::Input(message)).is_ok())
            };
            #[cfg(windows)]
            {
                crate::terminal::host_input::run_input_loop(pending, send);
            }
            #[cfg(not(windows))]
            {
                for event in pending {
                    if !send(event) {
                        return;
                    }
                }
                while let Ok(event) = ratatui::crossterm::event::read() {
                    if !send(event) {
                        break;
                    }
                }
            }
        })?;
    Ok(())
}

fn start_due_links(machines: &mut HashMap<String, MachineRuntime>, events: &Sender<ShellEvent>) {
    let now = Instant::now();
    for machine in machines.values_mut() {
        if !matches!(machine.state, MachineState::Reconnecting { at } if at <= now) {
            continue;
        }
        machine.generation = machine.generation.saturating_add(1);
        if let Some(reader) = machine.reader.take() {
            let _ = reader.join();
        }
        let shell = events.clone();
        match link::start(&machine.profile, machine.generation, move |event| {
            shell.send(ShellEvent::Link(event)).is_ok()
        }) {
            Ok(LinkTask { control, reader }) => {
                machine.control = Some(control);
                machine.reader = Some(reader);
                machine.state = MachineState::Connecting {
                    deadline: now + LINK_HANDSHAKE_TIMEOUT,
                };
            }
            Err(error) => {
                machine.state = MachineState::Attention(error.to_string());
            }
        }
    }
}

fn expire_connecting(machines: &mut HashMap<String, MachineRuntime>) {
    let now = Instant::now();
    for machine in machines.values_mut() {
        if matches!(machine.state, MachineState::Connecting { deadline } if deadline <= now) {
            stop_link(machine);
            machine.state = MachineState::Reconnecting {
                at: now + reconnect_delay(&machine.profile.id, machine.generation, machine.backoff),
            };
            machine.backoff = (machine.backoff * 2).min(RECONNECT_MAX);
        }
    }
}

fn next_deadline(
    machines: &HashMap<String, MachineRuntime>,
    candidate: Option<&SurfaceCandidate>,
) -> Option<Instant> {
    machines
        .values()
        .filter_map(|machine| match machine.state {
            MachineState::Connecting { deadline } => Some(deadline),
            MachineState::Reconnecting { at } => Some(at),
            _ => None,
        })
        .chain(candidate.map(|candidate| candidate.deadline))
        .min()
}

fn expire_candidate(
    candidate: &mut Option<SurfaceCandidate>,
    machines: &HashMap<String, MachineRuntime>,
) -> bool {
    if candidate
        .as_ref()
        .is_some_and(|candidate| candidate.deadline <= Instant::now())
    {
        if let Some(expired) = candidate.take() {
            close_remote(&expired.endpoint, machines);
        }
        true
    } else {
        false
    }
}

fn reconnect_delay(id: &str, generation: u64, base: Duration) -> Duration {
    let hash = id.bytes().fold(generation, |hash, byte| {
        hash.wrapping_mul(0x100000001b3)
            .wrapping_add(u64::from(byte))
    });
    let percent = 80 + hash % 41;
    Duration::from_millis((base.as_millis() as u64).saturating_mul(percent) / 100)
}

#[allow(clippy::too_many_arguments)]
fn handle_link_event(
    event: LinkEvent,
    machines: &mut HashMap<String, MachineRuntime>,
    active: &mut Endpoint,
    warm: &mut Option<Endpoint>,
    candidate: &mut Option<SurfaceCandidate>,
    next_request: &mut u64,
    local_writer: &Arc<Mutex<crate::ipc::transport::Conn>>,
    terminal: &mut DefaultTerminal,
    truecolor: bool,
    dock: &mut DockState,
    last_cursor: &mut Option<(u16, u16)>,
    cursor_visible: &mut bool,
    dock_rows: u16,
    next_channel: &mut u64,
) -> Result<Option<super::client::ClientExit>> {
    match event {
        LinkEvent::Message {
            machine_id,
            generation,
            message,
        } => {
            let Some(machine) = machines.get_mut(&machine_id) else {
                return Ok(None);
            };
            if machine.generation != generation {
                return Ok(None);
            }
            match message {
                fleet::ServerMessage::Welcome {
                    version,
                    server_version,
                    error,
                } => {
                    if version != fleet::FLEET_PROTOCOL_VERSION || error.is_some() {
                        stop_link(machine);
                        machine.state = MachineState::Attention(
                            error.unwrap_or_else(|| "fleet protocol mismatch".to_string()),
                        );
                    } else if server_version != env!("CARGO_PKG_VERSION") {
                        stop_link(machine);
                        machine.state = MachineState::Attention(format!(
                            "remote Luvus {server_version} does not match local {}",
                            env!("CARGO_PKG_VERSION")
                        ));
                    } else {
                        machine.state = MachineState::Online;
                        machine.backoff = Duration::from_secs(1);
                        if let Some(control) = machine.control.as_ref() {
                            control.send(&fleet::ClientMessage::Sessions {
                                request_id: *next_request,
                            })?;
                            *next_request = next_request.saturating_add(1);
                        }
                    }
                    dock.dirty = true;
                }
                fleet::ServerMessage::Sessions { sessions, .. } => {
                    machine.sessions = sessions;
                    dock.dirty = true;
                }
                fleet::ServerMessage::SurfaceOpened {
                    channel_id,
                    session,
                } => {
                    if candidate.as_ref().is_some_and(|candidate| {
                        candidate.endpoint
                            == Endpoint::Remote {
                                machine_id: machine_id.clone(),
                                channel_id,
                                session: session.clone(),
                            }
                            && candidate.generation == generation
                    }) {
                        let control = machine
                            .control
                            .as_ref()
                            .ok_or_else(|| anyhow!("machine link closed"))?;
                        let size = terminal.size()?;
                        control.send(&fleet::ClientMessage::Surface {
                            channel_id,
                            message: ClientMessage::Hello {
                                version: PROTOCOL_VERSION,
                                cols: size.width,
                                rows: size.height,
                            },
                        })?;
                    }
                }
                fleet::ServerMessage::Surface {
                    channel_id,
                    message,
                } => {
                    let Some(endpoint) = endpoint_for_channel(
                        &machine_id,
                        channel_id,
                        active,
                        warm.as_ref(),
                        candidate.as_ref(),
                    ) else {
                        return Ok(None);
                    };
                    if let Some(exit) = handle_surface_message(
                        endpoint,
                        message,
                        active,
                        warm,
                        candidate,
                        machines,
                        local_writer,
                        terminal,
                        truecolor,
                        dock,
                        last_cursor,
                        cursor_visible,
                        dock_rows,
                        next_channel,
                    )? {
                        return Ok(Some(exit));
                    }
                }
                fleet::ServerMessage::SurfaceClosed { channel_id, .. } => {
                    let closed = |endpoint: &Endpoint| matches!(endpoint, Endpoint::Remote { machine_id: id, channel_id: channel, .. } if id == &machine_id && *channel == channel_id);
                    if candidate
                        .as_ref()
                        .is_some_and(|candidate| closed(&candidate.endpoint))
                    {
                        *candidate = None;
                    }
                    if warm.as_ref().is_some_and(&closed) {
                        *warm = None;
                    }
                    if closed(active) && candidate.is_none() {
                        request_switch(
                            Endpoint::Local,
                            warm,
                            candidate,
                            next_channel,
                            machines,
                            local_writer,
                        )?;
                    }
                }
                fleet::ServerMessage::Error {
                    channel_id,
                    message,
                    ..
                } => {
                    let candidate_failed = channel_id.is_some_and(|channel| {
                        candidate.as_ref().is_some_and(|candidate| {
                            matches!(
                                &candidate.endpoint,
                                Endpoint::Remote {
                                    machine_id: id,
                                    channel_id,
                                    ..
                                } if id == &machine_id && *channel_id == channel
                            )
                        })
                    });
                    if candidate_failed {
                        *candidate = None;
                        dock.warning = Some(message);
                    } else {
                        machine.state = MachineState::Attention(message);
                    }
                    dock.dirty = true;
                }
                fleet::ServerMessage::Pong { .. } => {}
            }
        }
        LinkEvent::Disconnected {
            machine_id,
            generation,
            reason,
        } => {
            let Some(machine) = machines.get_mut(&machine_id) else {
                return Ok(None);
            };
            if machine.generation != generation {
                return Ok(None);
            }
            let failed_during_handshake = matches!(machine.state, MachineState::Connecting { .. });
            stop_link(machine);
            if !machine.profile.enabled {
                machine.state = MachineState::Disabled;
            } else if failed_during_handshake || reason != "SSH link closed" {
                machine.state = MachineState::Attention(if failed_during_handshake {
                    format!(
                        "connection failed; run `luvus machine status {}`",
                        machine.profile.id
                    )
                } else {
                    reason
                });
            } else {
                machine.state = MachineState::Reconnecting {
                    at: Instant::now() + reconnect_delay(&machine_id, generation, machine.backoff),
                };
                machine.backoff = (machine.backoff * 2).min(RECONNECT_MAX);
            }
            if matches!(active, Endpoint::Remote { machine_id: id, .. } if id == &machine_id) {
                *candidate = Some(SurfaceCandidate {
                    endpoint: Endpoint::Local,
                    generation: 0,
                    welcomed: true,
                    ready: true,
                    shell_dock: None,
                    deadline: Instant::now() + SURFACE_PREPARE_TIMEOUT,
                });
                send_local(
                    local_writer,
                    &ClientMessage::SurfaceInterest(SurfaceInterest::Prepared),
                )?;
                if let Ok(size) = terminal.size() {
                    send_local(
                        local_writer,
                        &ClientMessage::Resize {
                            cols: size.width,
                            rows: size.height,
                        },
                    )?;
                }
            }
            dock.dirty = true;
        }
    }
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
fn handle_surface_message(
    endpoint: Endpoint,
    message: ServerMessage,
    active: &mut Endpoint,
    warm: &mut Option<Endpoint>,
    candidate: &mut Option<SurfaceCandidate>,
    machines: &HashMap<String, MachineRuntime>,
    local_writer: &Arc<Mutex<crate::ipc::transport::Conn>>,
    terminal: &mut DefaultTerminal,
    truecolor: bool,
    dock: &mut DockState,
    last_cursor: &mut Option<(u16, u16)>,
    cursor_visible: &mut bool,
    dock_rows: u16,
    next_channel: &mut u64,
) -> Result<Option<super::client::ClientExit>> {
    match message {
        ServerMessage::Welcome { error, .. } => {
            if let Some(candidate) = candidate
                .as_mut()
                .filter(|candidate| candidate.endpoint == endpoint)
            {
                if let Some(error) = error {
                    return Err(anyhow!(error));
                }
                candidate.welcomed = true;
            }
        }
        ServerMessage::Ready { probe_terminal } => {
            if let Some(candidate) = candidate
                .as_mut()
                .filter(|candidate| candidate.endpoint == endpoint)
            {
                candidate.ready = true;
                if let Endpoint::Remote {
                    machine_id,
                    channel_id,
                    ..
                } = &endpoint
                {
                    let control = machines
                        .get(machine_id)
                        .and_then(|machine| machine.control.as_ref())
                        .ok_or_else(|| anyhow!("machine link closed during surface negotiation"))?;
                    if probe_terminal {
                        let probe = crate::terminal::theme_probe::probe();
                        control.send(&fleet::ClientMessage::Surface {
                            channel_id: *channel_id,
                            message: ClientMessage::TerminalColors(probe.colors),
                        })?;
                    }
                    control.send(&fleet::ClientMessage::Surface {
                        channel_id: *channel_id,
                        message: ClientMessage::ShellDockRows(dock_rows),
                    })?;
                    control.send(&fleet::ClientMessage::Surface {
                        channel_id: *channel_id,
                        message: ClientMessage::SurfaceInterest(SurfaceInterest::Prepared),
                    })?;
                }
            }
        }
        ServerMessage::ShellDock(rect) => {
            if endpoint == *active {
                dock.rect = rect;
                dock.dirty = true;
            } else if let Some(candidate) = candidate
                .as_mut()
                .filter(|candidate| candidate.endpoint == endpoint)
            {
                candidate.shell_dock = Some(rect);
            }
        }
        ServerMessage::OpenMachineSelector if endpoint == *active => {
            open_selector(dock, active, machines);
        }
        ServerMessage::Frame(frame) => {
            let is_candidate = candidate.as_ref().is_some_and(|candidate| {
                candidate.endpoint == endpoint && candidate.welcomed && candidate.ready
            });
            if (endpoint == *active && !dock.selector_open) || is_candidate {
                if is_candidate {
                    dock.rect = candidate
                        .as_ref()
                        .and_then(|candidate| candidate.shell_dock)
                        .flatten();
                }
                cache_dock_style(dock, &frame, truecolor);
                super::client::sync_begin();
                super::client::paint(
                    terminal,
                    &super::client::frame_cells(&frame, truecolor),
                    frame.cursor,
                    frame.cursor_visible,
                    true,
                    last_cursor,
                )?;
                super::client::sync_end();
                *cursor_visible = frame.cursor_visible;
                dock.dirty = true;
                if is_candidate {
                    commit_candidate(endpoint, active, warm, candidate, machines, local_writer)?;
                }
            }
        }
        ServerMessage::FrameDiff(diff) if endpoint == *active && !dock.selector_open => {
            super::client::sync_begin();
            super::client::paint(
                terminal,
                &super::client::diff_cells(&diff, truecolor),
                diff.cursor,
                diff.cursor_visible,
                false,
                last_cursor,
            )?;
            super::client::sync_end();
            *cursor_visible = diff.cursor_visible;
        }
        ServerMessage::Notify(message) if endpoint == *active => crate::emit_notification(&message),
        ServerMessage::Sound(signal) if endpoint == *active => crate::emit_sound(signal),
        ServerMessage::Clipboard(text) if endpoint == *active => crate::emit_clipboard(&text),
        ServerMessage::OpenUrl(url) if endpoint == *active => crate::platform::open_url(&url),
        ServerMessage::SwitchSession { name } if endpoint == *active => match &endpoint {
            Endpoint::Local => {
                return Ok(Some(super::client::ClientExit::SwitchSession(name)));
            }
            Endpoint::Remote { machine_id, .. } => {
                let generation = machines
                    .get(machine_id)
                    .map_or(0, |machine| machine.generation);
                request_switch(
                    Endpoint::Remote {
                        machine_id: machine_id.clone(),
                        channel_id: 0,
                        session: name,
                    },
                    warm,
                    candidate,
                    next_channel,
                    machines,
                    local_writer,
                )?;
                if let Some(candidate) = candidate.as_mut() {
                    candidate.generation = generation;
                }
            }
        },
        ServerMessage::Detach if endpoint == *active => {
            return Ok(Some(super::client::ClientExit::Detached));
        }
        ServerMessage::ServerShutdown { .. } if endpoint == *active => {
            return Ok(Some(super::client::ClientExit::ServerStopped));
        }
        _ => {}
    }
    Ok(None)
}

fn cache_dock_style(dock: &mut DockState, frame: &protocol::FrameData, truecolor: bool) {
    let (x, y) = dock.rect.map_or((0, 0), |rect| (rect.x, rect.y));
    let index = usize::from(y)
        .saturating_mul(usize::from(frame.width))
        .saturating_add(usize::from(x));
    if let Some(cell) = frame.cells.get(index) {
        dock.base = super::client::make_cell(&cell.symbol, cell.fg, cell.bg, cell.mods, truecolor);
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_dock_input(
    message: &ClientMessage,
    dock: &mut DockState,
    active: &Endpoint,
    warm: &mut Option<Endpoint>,
    candidate: &mut Option<SurfaceCandidate>,
    next_channel: &mut u64,
    machines: &HashMap<String, MachineRuntime>,
    local_writer: &Arc<Mutex<crate::ipc::transport::Conn>>,
    terminal: &mut DefaultTerminal,
) -> Result<bool> {
    if dock.selector_open {
        match message {
            ClientMessage::Key(key) => {
                let endpoints = selector_endpoints(machines);
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        close_selector(dock, active, machines, local_writer)?;
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        dock.selector_cursor = dock.selector_cursor.saturating_sub(1);
                        dock.dirty = true;
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if !endpoints.is_empty() {
                            dock.selector_cursor =
                                (dock.selector_cursor + 1).min(endpoints.len() - 1);
                        }
                        dock.dirty = true;
                    }
                    KeyCode::Home => {
                        dock.selector_cursor = 0;
                        dock.dirty = true;
                    }
                    KeyCode::End => {
                        dock.selector_cursor = endpoints.len().saturating_sub(1);
                        dock.dirty = true;
                    }
                    KeyCode::Char('r') => {
                        dock.refresh_catalog = true;
                        dock.dirty = true;
                    }
                    KeyCode::Enter => {
                        if let Some(endpoint) = endpoints.get(dock.selector_cursor) {
                            if !same_selection(endpoint, active) {
                                request_switch(
                                    endpoint.clone(),
                                    warm,
                                    candidate,
                                    next_channel,
                                    machines,
                                    local_writer,
                                )?;
                            }
                        }
                        close_selector(dock, active, machines, local_writer)?;
                    }
                    _ => {}
                }
            }
            ClientMessage::Mouse(mouse)
                if mouse.kind == MouseEventKind::Down(MouseButton::Left) =>
            {
                let hit = dock
                    .hits
                    .iter()
                    .find(|(_, hit)| hit.contains((mouse.column, mouse.row).into()))
                    .map(|(endpoint, _)| endpoint.clone());
                if let Some(endpoint) = hit {
                    if !same_selection(&endpoint, active) {
                        request_switch(
                            endpoint,
                            warm,
                            candidate,
                            next_channel,
                            machines,
                            local_writer,
                        )?;
                    }
                    close_selector(dock, active, machines, local_writer)?;
                } else if !dock
                    .selector_rect
                    .is_some_and(|rect| rect.contains((mouse.column, mouse.row).into()))
                {
                    close_selector(dock, active, machines, local_writer)?;
                }
            }
            ClientMessage::Resize { .. } => {
                dock.dirty = true;
                return Ok(false);
            }
            _ => {}
        }
        if dock.dirty {
            paint_dock(terminal, dock, machines, active, &mut None, false)?;
        }
        return Ok(true);
    }
    let ClientMessage::Mouse(mouse) = message else {
        return Ok(false);
    };
    let Some(rect) = dock.rect else {
        return Ok(false);
    };
    if mouse.column < rect.x
        || mouse.column >= rect.x.saturating_add(rect.width)
        || mouse.row < rect.y
        || mouse.row >= rect.y.saturating_add(rect.height)
    {
        return Ok(false);
    }
    if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
        if mouse.row == rect.y {
            open_selector(dock, active, machines);
            return Ok(true);
        }
        if let Some((endpoint, _)) = dock
            .hits
            .iter()
            .find(|(_, hit)| hit.contains((mouse.column, mouse.row).into()))
        {
            if !same_selection(endpoint, active) {
                request_switch(
                    endpoint.clone(),
                    warm,
                    candidate,
                    next_channel,
                    machines,
                    local_writer,
                )?;
            }
        }
    }
    Ok(true)
}

fn selector_endpoints(machines: &HashMap<String, MachineRuntime>) -> Vec<Endpoint> {
    let mut rows = machines.values().collect::<Vec<_>>();
    rows.sort_unstable_by(|left, right| left.profile.label.cmp(&right.profile.label));
    std::iter::once(Endpoint::Local)
        .chain(rows.into_iter().flat_map(|machine| {
            let sessions = if machine.sessions.is_empty() {
                vec![machine.session()]
            } else {
                machine
                    .sessions
                    .iter()
                    .map(|session| session.name.clone())
                    .collect()
            };
            sessions.into_iter().map(|session| Endpoint::Remote {
                machine_id: machine.profile.id.clone(),
                channel_id: 0,
                session,
            })
        }))
        .collect()
}

fn open_selector(
    dock: &mut DockState,
    active: &Endpoint,
    machines: &HashMap<String, MachineRuntime>,
) {
    let endpoints = selector_endpoints(machines);
    dock.selector_cursor = endpoints
        .iter()
        .position(|endpoint| same_selection(endpoint, active))
        .unwrap_or(0);
    dock.selector_open = true;
    dock.refresh_catalog = true;
    dock.dirty = true;
}

#[allow(clippy::too_many_arguments)]
fn refresh_catalog(
    machines: &mut HashMap<String, MachineRuntime>,
    active: &mut Endpoint,
    warm: &mut Option<Endpoint>,
    candidate: &mut Option<SurfaceCandidate>,
    next_channel: &mut u64,
    local_writer: &Arc<Mutex<crate::ipc::transport::Conn>>,
    dock: &mut DockState,
    dock_rows: &mut u16,
) -> Result<()> {
    let loaded = match crate::machine::catalog::load() {
        Ok(loaded) => loaded,
        Err(error) => {
            dock.warning = Some(error.to_string());
            dock.dirty = true;
            return Ok(());
        }
    };
    dock.warning = (!loaded.warnings.is_empty()).then(|| loaded.warnings.join("; "));
    let next = loaded
        .catalog
        .machines
        .into_iter()
        .map(|profile| (profile.id.clone(), profile))
        .collect::<HashMap<_, _>>();
    let replaced = machines
        .iter()
        .filter(|(id, runtime)| {
            next.get(*id)
                .is_none_or(|profile| profile != &runtime.profile)
        })
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();

    for id in &replaced {
        if candidate.as_ref().is_some_and(|candidate| {
            matches!(&candidate.endpoint, Endpoint::Remote { machine_id, .. } if machine_id == id)
        }) {
            if let Some(candidate) = candidate.take() {
                close_remote(&candidate.endpoint, machines);
            }
        }
        if warm.as_ref().is_some_and(
            |endpoint| matches!(endpoint, Endpoint::Remote { machine_id, .. } if machine_id == id),
        ) {
            if let Some(endpoint) = warm.take() {
                close_remote(&endpoint, machines);
            }
        }
        if let Some(mut runtime) = machines.remove(id) {
            stop_link(&mut runtime);
        }
    }

    let active_removed =
        matches!(active, Endpoint::Remote { machine_id, .. } if replaced.contains(machine_id));
    for (id, profile) in next {
        machines.entry(id).or_insert_with(|| {
            let state = if profile.enabled {
                MachineState::Reconnecting { at: Instant::now() }
            } else {
                MachineState::Disabled
            };
            MachineRuntime {
                profile,
                state,
                generation: 0,
                control: None,
                reader: None,
                backoff: Duration::from_secs(1),
                sessions: Vec::new(),
            }
        });
    }
    if active_removed {
        request_switch(
            Endpoint::Local,
            warm,
            candidate,
            next_channel,
            machines,
            local_writer,
        )?;
    }
    let rows = dock_row_count(machines.len());
    if rows != *dock_rows {
        *dock_rows = rows;
        send_local(local_writer, &ClientMessage::ShellDockRows(rows))?;
        if !matches!(active, Endpoint::Local) {
            let _ = send_surface(
                active,
                &ClientMessage::ShellDockRows(rows),
                machines,
                local_writer,
            );
        }
    }
    dock.dirty = true;
    Ok(())
}

fn close_selector(
    dock: &mut DockState,
    active: &Endpoint,
    machines: &HashMap<String, MachineRuntime>,
    local_writer: &Arc<Mutex<crate::ipc::transport::Conn>>,
) -> Result<()> {
    dock.selector_open = false;
    dock.selector_rect = None;
    dock.dirty = true;
    // Frames received beneath the client-owned selector were intentionally not
    // painted. Re-entering Active through Prepared invalidates the server-side
    // baseline and guarantees one complete frame before later diffs.
    send_interest(active, SurfaceInterest::Prepared, machines, local_writer)?;
    if let Ok((cols, rows)) = crossterm::terminal::size() {
        send_surface(
            active,
            &ClientMessage::Resize { cols, rows },
            machines,
            local_writer,
        )?;
    }
    send_interest(active, SurfaceInterest::Active, machines, local_writer)?;
    Ok(())
}

fn same_selection(left: &Endpoint, right: &Endpoint) -> bool {
    match (left, right) {
        (Endpoint::Local, Endpoint::Local) => true,
        (
            Endpoint::Remote {
                machine_id: left_machine,
                session: left_session,
                ..
            },
            Endpoint::Remote {
                machine_id: right_machine,
                session: right_session,
                ..
            },
        ) => left_machine == right_machine && left_session == right_session,
        _ => false,
    }
}

fn request_switch(
    mut endpoint: Endpoint,
    warm: &mut Option<Endpoint>,
    candidate: &mut Option<SurfaceCandidate>,
    next_channel: &mut u64,
    machines: &HashMap<String, MachineRuntime>,
    local_writer: &Arc<Mutex<crate::ipc::transport::Conn>>,
) -> Result<()> {
    if let Some(previous) = candidate.take() {
        if previous.endpoint != endpoint {
            close_remote(&previous.endpoint, machines);
        }
    }
    let warm_matches = warm.as_ref().is_some_and(|existing| {
        same_selection(existing, &endpoint)
            && match existing {
                Endpoint::Local => true,
                Endpoint::Remote {
                    machine_id,
                    channel_id,
                    ..
                } => {
                    *channel_id != 0
                        && machines
                            .get(machine_id)
                            .is_some_and(|machine| matches!(machine.state, MachineState::Online))
                }
            }
    });
    if warm_matches {
        endpoint = warm.take().expect("matching warm endpoint exists");
        send_interest(&endpoint, SurfaceInterest::Prepared, machines, local_writer)?;
        if let Ok((cols, rows)) = crossterm::terminal::size() {
            send_surface(
                &endpoint,
                &ClientMessage::Resize { cols, rows },
                machines,
                local_writer,
            )?;
        }
        let generation = match &endpoint {
            Endpoint::Local => 0,
            Endpoint::Remote { machine_id, .. } => machines
                .get(machine_id)
                .map_or(0, |machine| machine.generation),
        };
        *candidate = Some(SurfaceCandidate {
            endpoint,
            generation,
            welcomed: true,
            ready: true,
            shell_dock: None,
            deadline: Instant::now() + SURFACE_PREPARE_TIMEOUT,
        });
        return Ok(());
    }
    match &mut endpoint {
        Endpoint::Local => {
            send_local(
                local_writer,
                &ClientMessage::SurfaceInterest(SurfaceInterest::Prepared),
            )?;
            if let Ok((cols, rows)) = crossterm::terminal::size() {
                send_local(local_writer, &ClientMessage::Resize { cols, rows })?;
            }
            *candidate = Some(SurfaceCandidate {
                endpoint,
                generation: 0,
                welcomed: true,
                ready: true,
                shell_dock: None,
                deadline: Instant::now() + SURFACE_PREPARE_TIMEOUT,
            });
        }
        Endpoint::Remote {
            machine_id,
            channel_id,
            session,
        } => {
            let machine = machines
                .get(machine_id)
                .ok_or_else(|| anyhow!("unknown machine `{machine_id}`"))?;
            if !matches!(machine.state, MachineState::Online) {
                return Ok(());
            }
            *channel_id = *next_channel;
            *next_channel = next_channel.saturating_add(1).max(1);
            let control = machine
                .control
                .as_ref()
                .ok_or_else(|| anyhow!("machine link is not ready"))?;
            control.send(&fleet::ClientMessage::OpenSurface {
                channel_id: *channel_id,
                session: session.clone(),
            })?;
            *candidate = Some(SurfaceCandidate {
                endpoint,
                generation: machine.generation,
                welcomed: false,
                ready: false,
                shell_dock: None,
                deadline: Instant::now() + SURFACE_PREPARE_TIMEOUT,
            });
        }
    }
    Ok(())
}

fn endpoint_for_channel(
    machine_id: &str,
    channel_id: u64,
    active: &Endpoint,
    warm: Option<&Endpoint>,
    candidate: Option<&SurfaceCandidate>,
) -> Option<Endpoint> {
    candidate
        .map(|candidate| &candidate.endpoint)
        .into_iter()
        .chain(std::iter::once(active))
        .chain(warm)
        .find(|endpoint| {
            matches!(
                endpoint,
                Endpoint::Remote {
                    machine_id: id,
                    channel_id: channel,
                    ..
                } if id == machine_id && *channel == channel_id
            )
        })
        .cloned()
}

fn commit_candidate(
    endpoint: Endpoint,
    active: &mut Endpoint,
    warm: &mut Option<Endpoint>,
    candidate: &mut Option<SurfaceCandidate>,
    machines: &HashMap<String, MachineRuntime>,
    local_writer: &Arc<Mutex<crate::ipc::transport::Conn>>,
) -> Result<()> {
    send_interest(&endpoint, SurfaceInterest::Active, machines, local_writer)?;
    // The previous surface may close while the candidate is preparing. Once a
    // complete candidate frame is valid, a stale route must not veto commit.
    let _ = send_interest(active, SurfaceInterest::Suspended, machines, local_writer);
    if let Some(previous_warm) = warm.take() {
        if previous_warm != *active && previous_warm != Endpoint::Local {
            close_remote(&previous_warm, machines);
        }
    }
    *warm = Some(active.clone());
    *active = endpoint;
    *candidate = None;
    Ok(())
}

fn send_surface(
    endpoint: &Endpoint,
    message: &ClientMessage,
    machines: &HashMap<String, MachineRuntime>,
    local_writer: &Arc<Mutex<crate::ipc::transport::Conn>>,
) -> Result<()> {
    match endpoint {
        Endpoint::Local => send_local(local_writer, message),
        Endpoint::Remote {
            machine_id,
            channel_id,
            ..
        } => machines
            .get(machine_id)
            .and_then(|machine| machine.control.as_ref())
            .ok_or_else(|| anyhow!("active machine link is unavailable"))?
            .send(&fleet::ClientMessage::Surface {
                channel_id: *channel_id,
                message: message.clone(),
            }),
    }
}

fn send_interest(
    endpoint: &Endpoint,
    interest: SurfaceInterest,
    machines: &HashMap<String, MachineRuntime>,
    local_writer: &Arc<Mutex<crate::ipc::transport::Conn>>,
) -> Result<()> {
    send_surface(
        endpoint,
        &ClientMessage::SurfaceInterest(interest),
        machines,
        local_writer,
    )
}

fn send_local(
    writer: &Arc<Mutex<crate::ipc::transport::Conn>>,
    message: &ClientMessage,
) -> Result<()> {
    let mut writer = writer.lock().unwrap_or_else(|error| error.into_inner());
    protocol::write_message(&mut *writer, message)?;
    Ok(())
}

fn close_remote(endpoint: &Endpoint, machines: &HashMap<String, MachineRuntime>) {
    if let Endpoint::Remote {
        machine_id,
        channel_id,
        ..
    } = endpoint
    {
        if let Some(control) = machines
            .get(machine_id)
            .and_then(|machine| machine.control.as_ref())
        {
            let _ = control.send(&fleet::ClientMessage::CloseSurface {
                channel_id: *channel_id,
            });
        }
    }
}

fn stop_link(machine: &mut MachineRuntime) {
    if let Some(control) = machine.control.take() {
        control.close();
    }
    if let Some(reader) = machine.reader.take() {
        let _ = reader.join();
    }
}

fn paint_dock(
    terminal: &mut DefaultTerminal,
    dock: &mut DockState,
    machines: &HashMap<String, MachineRuntime>,
    active: &Endpoint,
    cursor: &mut Option<(u16, u16)>,
    cursor_visible: bool,
) -> Result<()> {
    if dock.selector_open {
        return paint_selector(terminal, dock, machines, active, cursor);
    }
    let Some(rect) = dock.rect else {
        dock.dirty = false;
        return Ok(());
    };
    let mut cells = Vec::with_capacity(usize::from(rect.width) * usize::from(rect.height));
    for y in rect.y..rect.y.saturating_add(rect.height) {
        for x in rect.x..rect.x.saturating_add(rect.width) {
            let mut cell = dock.base.clone();
            cell.set_symbol(" ");
            cells.push((x, y, cell));
        }
    }
    dock.hits.clear();
    write_row(
        &mut cells,
        rect,
        0,
        &dock.heading.to_uppercase(),
        if dock.warning.is_some() {
            "attention"
        } else {
            ""
        },
        &dock.base,
        Color::Reset,
    );
    if rect.height > 1 {
        let local_active = matches!(active, Endpoint::Local);
        write_row(
            &mut cells,
            rect,
            1,
            if local_active {
                "● Local"
            } else {
                "○ Local"
            },
            "online",
            &dock.base,
            if local_active {
                Color::LightGreen
            } else {
                Color::Reset
            },
        );
        dock.hits.push((
            Endpoint::Local,
            Rect::new(rect.x, rect.y + 1, rect.width, 1),
        ));
    }
    let mut rows = machines.values().collect::<Vec<_>>();
    rows.sort_unstable_by(|left, right| left.profile.label.cmp(&right.profile.label));
    for (index, machine) in rows
        .into_iter()
        .take(rect.height.saturating_sub(2).div_ceil(2) as usize)
        .enumerate()
    {
        let row = index as u16 * 2 + 2;
        let endpoint = Endpoint::Remote {
            machine_id: machine.profile.id.clone(),
            channel_id: 0,
            session: machine.session(),
        };
        let selected = matches!(active, Endpoint::Remote { machine_id, .. } if machine_id == &machine.profile.id);
        let label = format!(
            "{} {}",
            if selected { "●" } else { "○" },
            machine.profile.label
        );
        let tone = match machine.state {
            MachineState::Online => Color::LightGreen,
            MachineState::Connecting { .. } | MachineState::Reconnecting { .. } => Color::Yellow,
            MachineState::Attention(_) => Color::LightRed,
            MachineState::Disabled => Color::DarkGray,
        };
        write_row(
            &mut cells,
            rect,
            row,
            &label,
            machine.status(),
            &dock.base,
            tone,
        );
        if row + 1 < rect.height {
            write_row(
                &mut cells,
                rect,
                row + 1,
                &format!("  {}", machine.session()),
                "",
                &dock.base,
                Color::Reset,
            );
        }
        dock.hits.push((
            endpoint,
            Rect::new(
                rect.x,
                rect.y + row,
                rect.width,
                rect.height.saturating_sub(row).min(2),
            ),
        ));
    }
    super::client::sync_begin();
    super::client::paint(terminal, &cells, *cursor, cursor_visible, false, cursor)?;
    super::client::sync_end();
    dock.dirty = false;
    Ok(())
}

fn paint_selector(
    terminal: &mut DefaultTerminal,
    dock: &mut DockState,
    machines: &HashMap<String, MachineRuntime>,
    active: &Endpoint,
    cursor: &mut Option<(u16, u16)>,
) -> Result<()> {
    let size = terminal.size()?;
    let mobile = size.width <= crate::app::MOBILE_WIDTH;
    let endpoints = selector_endpoints(machines);
    dock.selector_cursor = dock.selector_cursor.min(endpoints.len().saturating_sub(1));
    let row_height = if mobile { 2 } else { 1 };
    let content_height = (endpoints.len() as u16)
        .saturating_mul(row_height)
        .saturating_add(3);
    let width = if mobile {
        size.width
    } else {
        size.width.clamp(20, 52)
    };
    let height = size.height.min(content_height.max(5));
    let rect = if mobile {
        Rect::new(0, 0, width, height)
    } else {
        Rect::new(
            size.width.saturating_sub(width) / 2,
            size.height.saturating_sub(height) / 2,
            width,
            height,
        )
    };
    dock.selector_rect = Some(rect);
    dock.hits.clear();
    let mut cells = Vec::with_capacity(usize::from(rect.width) * usize::from(rect.height));
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            let mut cell = dock.base.clone();
            cell.set_symbol(" ");
            cells.push((x, y, cell));
        }
    }
    let selector_status = if dock.warning.is_some() {
        "attention".to_string()
    } else {
        format!("esc {}", dock.close_label)
    };
    write_row(
        &mut cells,
        ShellDockRect {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        },
        0,
        &dock.heading.to_uppercase(),
        &selector_status,
        &dock.base,
        Color::Reset,
    );
    for (index, endpoint) in endpoints.iter().enumerate() {
        let row = 2 + index as u16 * row_height;
        if row >= rect.height {
            break;
        }
        let (label, status, tone) = match endpoint {
            Endpoint::Local => (
                "Local".to_string(),
                "online",
                if matches!(active, Endpoint::Local) {
                    Color::LightGreen
                } else {
                    Color::Reset
                },
            ),
            Endpoint::Remote {
                machine_id,
                session,
                ..
            } => {
                let machine = &machines[machine_id];
                let tone = match machine.state {
                    MachineState::Online => Color::LightGreen,
                    MachineState::Connecting { .. } | MachineState::Reconnecting { .. } => {
                        Color::Yellow
                    }
                    MachineState::Attention(_) => Color::LightRed,
                    MachineState::Disabled => Color::DarkGray,
                };
                (
                    if mobile {
                        machine.profile.label.clone()
                    } else {
                        format!("{} / {session}", machine.profile.label)
                    },
                    machine.status(),
                    tone,
                )
            }
        };
        let selected = index == dock.selector_cursor;
        let active_mark = if same_selection(endpoint, active) {
            "●"
        } else {
            "○"
        };
        write_row(
            &mut cells,
            ShellDockRect {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
            },
            row,
            &format!("{active_mark} {label}"),
            status,
            &dock.base,
            tone,
        );
        if selected {
            for (_, y, cell) in cells.iter_mut().filter(|(_, y, _)| *y == rect.y + row) {
                let _ = y;
                cell.modifier.insert(Modifier::REVERSED);
            }
        }
        if mobile && row + 1 < rect.height {
            let detail = match endpoint {
                Endpoint::Local => crate::session::display_name(),
                Endpoint::Remote { session, .. } => session.clone(),
            };
            write_row(
                &mut cells,
                ShellDockRect {
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: rect.height,
                },
                row + 1,
                &format!("  {detail}"),
                "",
                &dock.base,
                Color::Reset,
            );
            if selected {
                for (_, _, cell) in cells.iter_mut().filter(|(_, y, _)| *y == rect.y + row + 1) {
                    cell.modifier.insert(Modifier::REVERSED);
                }
            }
        }
        dock.hits.push((
            endpoint.clone(),
            Rect::new(rect.x, rect.y + row, rect.width, row_height),
        ));
    }
    super::client::sync_begin();
    super::client::paint(terminal, &cells, *cursor, false, false, cursor)?;
    super::client::sync_end();
    dock.dirty = false;
    Ok(())
}

fn write_row(
    cells: &mut [(u16, u16, Cell)],
    rect: ShellDockRect,
    row: u16,
    label: &str,
    status: &str,
    base: &Cell,
    tone: Color,
) {
    if row >= rect.height || rect.width < 2 {
        return;
    }
    let width = usize::from(rect.width.saturating_sub(2));
    let status_width = status.chars().count().min(width);
    let label_width = width.saturating_sub(status_width + usize::from(!status.is_empty()));
    let label = label.chars().take(label_width).collect::<String>();
    let mut text = label;
    if !status.is_empty() {
        let padding = width.saturating_sub(text.chars().count() + status_width);
        text.extend(std::iter::repeat_n(' ', padding));
        text.extend(status.chars().take(status_width));
    }
    for (offset, symbol) in text.chars().enumerate() {
        let x = rect.x + 1 + offset as u16;
        if let Some((_, _, cell)) = cells
            .iter_mut()
            .find(|(cell_x, cell_y, _)| *cell_x == x && *cell_y == rect.y + row)
        {
            *cell = base.clone();
            cell.set_symbol(&symbol.to_string());
            if tone != Color::Reset {
                cell.set_fg(tone);
            }
            if row == 0 {
                cell.modifier.insert(Modifier::BOLD);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_backoff_is_bounded_and_jittered() {
        let base = Duration::from_secs(10);
        let first = reconnect_delay("alpha", 1, base);
        let second = reconnect_delay("alpha", 2, base);
        assert!((Duration::from_secs(8)..=Duration::from_secs(12)).contains(&first));
        assert!((Duration::from_secs(8)..=Duration::from_secs(12)).contains(&second));
        assert_ne!(first, second);
    }

    #[test]
    fn disabled_machine_never_has_a_runtime_deadline() {
        let mut profile = MachineProfile::new("box".into(), "box".into());
        profile.enabled = false;
        let machines = HashMap::from([(
            profile.id.clone(),
            MachineRuntime {
                profile,
                state: MachineState::Disabled,
                generation: 0,
                control: None,
                reader: None,
                backoff: Duration::from_secs(1),
                sessions: Vec::new(),
            },
        )]);
        assert!(next_deadline(&machines, None).is_none());
    }

    #[test]
    fn compact_dock_is_absent_when_empty_and_uses_two_rows_per_machine() {
        assert_eq!(dock_row_count(0), 0);
        assert_eq!(dock_row_count(1), 4);
        assert_eq!(dock_row_count(5), 12);
        assert_eq!(dock_row_count(64), 12);
    }

    #[test]
    fn selector_qualifies_each_remote_named_session() {
        let profile = MachineProfile::new("box".into(), "box".into());
        let machines = HashMap::from([(
            profile.id.clone(),
            MachineRuntime {
                profile,
                state: MachineState::Online,
                generation: 1,
                control: None,
                reader: None,
                backoff: Duration::from_secs(1),
                sessions: vec![
                    fleet::SessionSummary {
                        name: "default".into(),
                        default: true,
                        running: true,
                    },
                    fleet::SessionSummary {
                        name: "review".into(),
                        default: false,
                        running: true,
                    },
                ],
            },
        )]);
        let endpoints = selector_endpoints(&machines);
        assert_eq!(endpoints.len(), 3);
        assert!(matches!(endpoints[0], Endpoint::Local));
        assert!(matches!(
            &endpoints[1],
            Endpoint::Remote { machine_id, session, .. }
                if machine_id == "box" && session == "default"
        ));
        assert!(matches!(
            &endpoints[2],
            Endpoint::Remote { machine_id, session, .. }
                if machine_id == "box" && session == "review"
        ));
        let active = Endpoint::Remote {
            machine_id: "box".into(),
            channel_id: 99,
            session: "review".into(),
        };
        assert!(same_selection(&endpoints[2], &active));
        assert!(!same_selection(&endpoints[1], &active));
    }

    #[test]
    fn channel_resolution_is_exact_and_prefers_the_new_candidate() {
        let active = Endpoint::Remote {
            machine_id: "alpha".into(),
            channel_id: 7,
            session: "old".into(),
        };
        let candidate = SurfaceCandidate {
            endpoint: Endpoint::Remote {
                machine_id: "alpha".into(),
                channel_id: 9,
                session: "new".into(),
            },
            generation: 2,
            welcomed: true,
            ready: true,
            shell_dock: None,
            deadline: Instant::now() + Duration::from_secs(1),
        };
        assert_eq!(
            endpoint_for_channel("alpha", 9, &active, None, Some(&candidate)),
            Some(candidate.endpoint.clone())
        );
        assert_eq!(
            endpoint_for_channel("alpha", 7, &active, None, Some(&candidate)),
            Some(active)
        );
        assert_eq!(
            endpoint_for_channel("alpha", 8, &Endpoint::Local, None, Some(&candidate)),
            None
        );
    }
}
