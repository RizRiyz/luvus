//! Thin-client remote-session shell.
//!
//! This path also handles an empty catalog so the first profile can be created
//! from the Workspaces `+` modal. The selected Luvus server still renders the
//! product UI; the shell owns only the owner-local profile form, endpoint rows
//! appended to Workspaces, direct native-protocol SSH endpoints, and
//! generation-fenced surface switching.

use std::collections::{HashMap, HashSet};
use std::io::BufReader;
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender as Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use ratatui::buffer::Cell;
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers, MouseButton,
    MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use ratatui::DefaultTerminal;

use crate::ipc::protocol::{
    self, ClientMessage, ServerMessage, ShellDockLayout, ShellDockRect, SurfaceInterest,
    PROTOCOL_VERSION,
};
use crate::machine::catalog::MachineProfile;
use crate::machine::link::{self, LinkControl, LinkEvent, LinkTask};

const LINK_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const LINK_HEALTH_IDLE: Duration = Duration::from_secs(5);
const LINK_HEALTH_TIMEOUT: Duration = Duration::from_secs(10);
const SURFACE_PREPARE_TIMEOUT: Duration = Duration::from_secs(15);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

enum ShellEvent {
    Started {
        machine_id: String,
        session: String,
        generation: u64,
        result: std::result::Result<LinkTask, String>,
    },
    Input(ClientMessage),
    Local(u64, ServerMessage),
    LocalClosed(u64),
    Link(LinkEvent),
    MachineCreated(std::result::Result<crate::machine::ProfileSetupOutcome, String>),
    MachineRemoved {
        machine_id: String,
        result: std::result::Result<(), String>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Endpoint {
    Local,
    Remote { machine_id: String, session: String },
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
    selected_session: String,
    runtime: SessionRuntime,
}

struct SessionRuntime {
    workspaces: Vec<protocol::ShellWorkspace>,
    state: MachineState,
    generation: u64,
    control: Option<LinkControl>,
    reader: Option<JoinHandle<()>>,
    backoff: Duration,
    handshake_failures: u8,
    last_evidence: Instant,
    health_probe: Option<(u64, Instant)>,
    welcomed: bool,
    ready: bool,
    boot_id: Option<u64>,
}

impl MachineRuntime {
    fn session(&self) -> String {
        self.selected_session.clone()
    }

    fn endpoint(&self, session: &str) -> Option<&SessionRuntime> {
        (self.selected_session == session).then_some(&self.runtime)
    }

    fn endpoint_mut(&mut self, session: &str) -> Option<&mut SessionRuntime> {
        (self.selected_session == session).then_some(&mut self.runtime)
    }
}

impl SessionRuntime {
    fn status(&self) -> &str {
        match &self.state {
            MachineState::Disabled => "disabled",
            MachineState::Connecting { .. } => "connecting",
            MachineState::Online => "online",
            MachineState::Reconnecting { .. } => "reconnecting",
            MachineState::Attention(_) => "attention",
        }
    }

    fn attention_reason(&self) -> Option<&str> {
        match &self.state {
            MachineState::Attention(reason) => Some(reason),
            _ => None,
        }
    }
}

impl Drop for SessionRuntime {
    fn drop(&mut self) {
        // Error exits must close their SSH children too, not only the normal
        // client-detach path. Reader workers own waiting and reaping.
        if let Some(control) = &self.control {
            control.close();
        }
    }
}

fn new_session_runtime(enabled: bool, generation: u64) -> SessionRuntime {
    SessionRuntime {
        workspaces: Vec::new(),
        state: if enabled {
            MachineState::Reconnecting { at: Instant::now() }
        } else {
            MachineState::Disabled
        },
        generation,
        control: None,
        reader: None,
        backoff: Duration::from_secs(1),
        handshake_failures: 0,
        last_evidence: Instant::now(),
        health_probe: None,
        welcomed: false,
        ready: false,
        boot_id: None,
    }
}

fn new_machine_runtime(profile: MachineProfile) -> MachineRuntime {
    let selected_session = profile.session_names().remove(0);
    let runtime = new_session_runtime(profile.enabled, 0);
    MachineRuntime {
        profile,
        selected_session,
        runtime,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DockHit {
    RemoteWorkspace(Endpoint, u16),
    AddWorkspace,
    LocalWorkspace(u16),
    Machine(String),
    Endpoint(Endpoint),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum MachineDockRow {
    LocalHeading,
    RemoteWorkspace(Endpoint, protocol::ShellWorkspace),
    LocalWorkspace(protocol::ShellWorkspace),
    Machine(String),
}

struct SurfaceCandidate {
    ticket: u64,
    endpoint: Endpoint,
    welcomed: bool,
    ready: bool,
    shell_dock: Option<Option<ShellDockRect>>,
    boot_id: Option<u64>,
    deadline: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MachineFormHit {
    OpenWorkspaceTab,
    RemoteMachineTab,
    Field(usize),
    ApproveInstall,
    DeclineInstall,
    Saved(usize),
    Submit,
    Cancel,
    Modal,
}

#[derive(Default)]
struct MachineForm {
    fields: [String; 3],
    cursor: usize,
    edit_cursors: [usize; 3],
    submitting: bool,
    error: Option<String>,
    install_prompt: Option<String>,
    selected_saved: Option<String>,
}

enum MachinePopup {
    Menu {
        machine_id: String,
        label: String,
        anchor: (u16, u16),
        rect: Option<Rect>,
        remove_rect: Option<Rect>,
        selected: usize,
    },
    Confirm {
        machine_id: String,
        label: String,
        rect: Option<Rect>,
        confirm_rect: Option<Rect>,
        cancel_rect: Option<Rect>,
        removing: bool,
        error: Option<String>,
    },
}

struct DockState {
    /// Cached host capability. Remote machine endpoints remain text-only until
    /// their image namespaces can be isolated from the local endpoint.
    graphics: bool,
    local_session: String,
    scroll: usize,
    rect: Option<ShellDockRect>,
    workspace_width: Option<u16>,
    sidebars: Option<protocol::ShellSidebars>,
    /// At most one client-owned sidebar revision waits for an endpoint echo.
    /// New drag positions replace the retained state instead of filling the
    /// SSH writer queue with obsolete mouse coordinates.
    sidebar_sync: Option<(Endpoint, u64)>,
    install_label: &'static str,
    install_action_label: &'static str,
    saved_labels: (&'static str, &'static str, &'static str),
    width_drag: Option<protocol::ShellResize>,
    selector_rect: Option<Rect>,
    selector_open: bool,
    selector_cursor: usize,
    refresh_catalog: bool,
    catalog_revision: u64,
    warning: Option<String>,
    heading: &'static str,
    close_label: &'static str,
    workspace_label: &'static str,
    remote_machine_label: &'static str,
    machine_name_label: &'static str,
    ssh_destination_label: &'static str,
    session_name_label: &'static str,
    create_label: &'static str,
    cancel_label: &'static str,
    delete_label: &'static str,
    working_label: &'static str,
    form_theme: protocol::MachineFormTheme,
    base: Cell,
    owner_style: Option<ShellDockRect>,
    owner_base: Option<Cell>,
    owner_session_slot: Option<Rect>,
    owner_session_button: Option<Rect>,
    owner_session_hovered: bool,
    pending_local_session_click: Option<ratatui::crossterm::event::MouseEvent>,
    local_workspaces: Vec<protocol::ShellWorkspace>,
    local_collapsed: bool,
    collapsed_machines: HashSet<String>,
    hits: Vec<(DockHit, Rect)>,
    hover: Option<DockHit>,
    navigation: Option<usize>,
    navigation_reveal: bool,
    focus_seen: bool,
    workspace_buffer: ratatui::buffer::Buffer,
    pending_rect: Option<Option<ShellDockRect>>,
    revealed_workspace: Option<(Endpoint, String, bool)>,
    pending_workspace_menu: Option<(Endpoint, String, u16, u16)>,
    machine_form: Option<MachineForm>,
    machine_draft: Option<MachineForm>,
    saved_ids: Vec<String>,
    machine_form_rect: Option<Rect>,
    machine_form_hits: Vec<(MachineFormHit, Rect)>,
    machine_form_backdrop_pending: bool,
    machine_popup: Option<MachinePopup>,
    dirty: bool,
}

impl Default for DockState {
    fn default() -> Self {
        Self {
            graphics: false,
            local_session: crate::session::display_name(),
            scroll: 0,
            rect: None,
            workspace_width: None,
            sidebars: None,
            sidebar_sync: None,
            install_label: crate::i18n::cli::machine_install_label(
                crate::i18n::cli::Context::configured().language(),
            ),
            install_action_label: crate::i18n::cli::machine_install_action_label(
                crate::i18n::cli::Context::configured().language(),
            ),
            saved_labels: crate::i18n::cli::machine_saved_labels(
                crate::i18n::cli::Context::configured().language(),
            ),
            width_drag: None,
            selector_rect: None,
            selector_open: false,
            selector_cursor: 0,
            refresh_catalog: false,
            catalog_revision: 0,
            warning: None,
            heading: crate::i18n::EN.machines,
            close_label: crate::i18n::EN.act_close,
            workspace_label: crate::i18n::EN.open_workspace,
            remote_machine_label: crate::i18n::EN.remote_machine,
            machine_name_label: crate::i18n::EN.machine_name,
            ssh_destination_label: crate::i18n::EN.ssh_destination,
            session_name_label: crate::i18n::EN.session_name,
            create_label: crate::i18n::EN.act_create,
            cancel_label: crate::i18n::EN.act_cancel,
            delete_label: crate::i18n::EN.act_delete,
            working_label: crate::i18n::EN.mc_working,
            form_theme: protocol::MachineFormTheme {
                surface: protocol::pack(Color::Reset),
                border: protocol::pack(Color::DarkGray),
                text: protocol::pack(Color::Reset),
                subtext0: protocol::pack(Color::DarkGray),
                subtext1: protocol::pack(Color::Gray),
                accent: protocol::pack(Color::Yellow),
                accent_text: protocol::pack(Color::Black),
                divider: protocol::pack(Color::DarkGray),
                rule: protocol::pack(Color::DarkGray),
                error: protocol::pack(Color::LightRed),
            },
            base: Cell::default(),
            owner_style: None,
            owner_base: None,
            owner_session_slot: None,
            owner_session_button: None,
            owner_session_hovered: false,
            pending_local_session_click: None,
            local_workspaces: Vec::new(),
            local_collapsed: false,
            collapsed_machines: HashSet::new(),
            hits: Vec::new(),
            hover: None,
            navigation: None,
            navigation_reveal: false,
            focus_seen: false,
            workspace_buffer: ratatui::buffer::Buffer::empty(Rect::default()),
            pending_rect: None,
            revealed_workspace: None,
            pending_workspace_menu: None,
            machine_form: None,
            machine_draft: None,
            saved_ids: Vec::new(),
            machine_form_rect: None,
            machine_form_hits: Vec::new(),
            machine_form_backdrop_pending: false,
            machine_popup: None,
            dirty: true,
        }
    }
}

impl DockState {
    fn overlay_open(&self) -> bool {
        self.selector_open || self.machine_form.is_some() || self.machine_popup.is_some()
    }
}

fn shell_block_contains(block: protocol::ShellDockBlock, x: u16, y: u16) -> bool {
    x >= block.x
        && x < block.x.saturating_add(block.width)
        && y >= block.y
        && y < block.y.saturating_add(block.height)
}

fn frame_diff_intersects_rect(diff: &protocol::FrameDiff, rect: Rect) -> bool {
    let width = u32::from(diff.width);
    if width == 0 || rect.is_empty() {
        return false;
    }
    diff.runs.iter().any(|run| {
        let run_start = run.start;
        let run_end = run_start.saturating_add(run.symbols.len() as u32);
        let first_row = u32::from(rect.y).max(run_start / width);
        let last_row = u32::from(rect.bottom()).min(run_end.saturating_add(width - 1) / width);
        (first_row..last_row).any(|row| {
            let rect_start = row.saturating_mul(width).saturating_add(u32::from(rect.x));
            let rect_end = rect_start.saturating_add(u32::from(rect.width));
            run_start < rect_end && run_end > rect_start
        })
    })
}

fn note_catalog_revision(dock: &mut DockState, revision: u64) -> bool {
    if revision <= dock.catalog_revision {
        return false;
    }
    dock.catalog_revision = revision;
    dock.refresh_catalog = true;
    true
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
        } => {
            return Err(anyhow!(
                "server: {error}\nAn older luvus server is likely still running — \
                 run `luvus server restart` to load this version (your session is saved)."
            ))
        }
        _ => return Err(anyhow!("unexpected local server handshake")),
    }
    let probe_colors = match protocol::read_message::<_, ServerMessage>(&mut reader)? {
        ServerMessage::Ready { probe_colors } => probe_colors,
        _ => return Err(anyhow!("unexpected local server negotiation")),
    };
    let probe = crate::terminal::theme_probe::probe(probe_colors);
    protocol::write_message(
        &mut writer,
        &ClientMessage::TerminalProbe {
            colors: probe.colors.clone(),
            graphics: probe.graphics,
            cell_size: probe.cell_size,
        },
    )?;
    protocol::write_message(&mut writer, &super::client::cell_pixels_message())?;

    let mut machines = profiles
        .into_iter()
        .map(|profile| (profile.id.clone(), new_machine_runtime(profile)))
        .collect::<HashMap<_, _>>();
    let mut active = Endpoint::Local;
    let mut dock_rows = projected_dock_row_count(&machines, &active, 0);
    protocol::write_message(
        &mut writer,
        &ClientMessage::ShellDockLayout(dock_layout(&active, &machines, 0)),
    )?;
    protocol::write_message(
        &mut writer,
        &ClientMessage::SurfaceInterest(SurfaceInterest::Active),
    )?;

    let writer = Arc::new(Mutex::new(writer));
    let (events_tx, events_rx) = mpsc::sync_channel(8);
    let local_generation = 0u64;
    start_local_reader(reader, events_tx.clone(), local_generation)?;

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
    // Crossterm's keyboard-enhancement query and event reader share one global
    // lock. Start input only after the query, matching the ordinary thin-client
    // path, or attachment can remain on a blank alternate screen until a key
    // event happens to release the reader.
    start_input_reader(probe.pending, events_tx.clone())?;

    let mut candidate: Option<SurfaceCandidate> = None;
    let mut pending_endpoint: Option<Endpoint> = None;
    let mut initial_local_frame_painted = false;
    let labels = crate::i18n::by_code(&crate::config::load().language);
    let mut dock = DockState {
        graphics: probe.graphics.unwrap_or(false),
        heading: labels.workspaces,
        close_label: labels.act_close,
        workspace_label: labels.open_workspace,
        remote_machine_label: labels.remote_machine,
        machine_name_label: labels.machine_name,
        ssh_destination_label: labels.ssh_destination,
        session_name_label: labels.session_name,
        create_label: labels.act_create,
        cancel_label: labels.act_cancel,
        delete_label: labels.act_delete,
        working_label: labels.mc_working,
        ..DockState::default()
    };
    let mut last_cursor = None;
    let mut cursor_visible = false;
    let mut exit = super::client::ClientExit::Done;
    let mut running = true;

    while running {
        // Enter the local product surface before launching SSH children. Process
        // startup can briefly block in the OS (notably during macOS executable
        // validation), and doing it while the alternate screen is still blank
        // makes ordinary local attachment look stalled.
        if initial_local_frame_painted {
            let size = terminal.size()?;
            start_due_links(&mut machines, &events_tx, size.width, size.height);
        }
        let unhealthy = service_endpoint_health(&mut machines);
        if unhealthy
            .iter()
            .any(|endpoint| same_selection(endpoint, &active))
        {
            prepare_local_failover(&mut candidate, &writer, terminal)?;
            dock.warning = Some("active machine connection timed out".to_string());
            dock.dirty = true;
        } else if candidate.as_ref().is_some_and(|candidate| {
            unhealthy
                .iter()
                .any(|endpoint| same_selection(endpoint, &candidate.endpoint))
        }) {
            candidate = None;
            dock.pending_local_session_click = None;
            dock.warning = Some("machine connection timed out while switching".to_string());
            dock.dirty = true;
        }
        expire_connecting(&mut machines);
        if expire_candidate(&mut candidate, &machines, &writer) {
            dock.pending_workspace_menu = None;
            dock.pending_local_session_click = None;
            set_machine_form_error(&mut dock, "Remote session preparation timed out.");
            dock.warning = Some("surface preparation timed out".to_string());
            dock.dirty = true;
        }
        // Settle client-owned work before parking, including input events
        // consumed by an overlay or sidebar. Such events need not produce a
        // server frame to wake us again.
        if dock.refresh_catalog {
            dock.refresh_catalog = false;
            refresh_catalog(
                &mut machines,
                &mut active,
                &mut candidate,
                &writer,
                &mut dock,
                &mut dock_rows,
            )?;
        }
        try_open_pending_endpoint(
            &mut pending_endpoint,
            &mut dock,
            &mut candidate,
            &mut machines,
            &writer,
        )?;
        sync_dock_rows(&machines, &active, &writer, &mut dock_rows, &mut dock)?;
        if candidate.is_none() && pending_endpoint.is_none() {
            if let Some((endpoint, workspace_id, col, row)) = dock.pending_workspace_menu.take() {
                if endpoint == active {
                    send_surface(
                        &endpoint,
                        &ClientMessage::ShellWorkspaceMenu {
                            workspace_id,
                            col,
                            row,
                        },
                        &machines,
                        &writer,
                    )?;
                }
            }
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
            ShellEvent::Started {
                machine_id,
                session,
                generation,
                result,
            } => {
                match result {
                    Ok(task) => {
                        if let Some(runtime) = machines
                            .get_mut(&machine_id)
                            .and_then(|machine| machine.endpoint_mut(&session))
                            .filter(|runtime| {
                                runtime.generation == generation
                                    && matches!(runtime.state, MachineState::Connecting { .. })
                            })
                        {
                            runtime.control = Some(task.control);
                            runtime.reader = Some(task.reader);
                            let size = terminal.size()?;
                            if let Err(error) =
                                runtime
                                    .control
                                    .as_ref()
                                    .unwrap()
                                    .send(&ClientMessage::Hello {
                                        version: PROTOCOL_VERSION,
                                        cols: size.width,
                                        rows: size.height,
                                    })
                            {
                                runtime.state = MachineState::Attention(error.to_string());
                            }
                        } else {
                            task.control.close();
                        }
                    }
                    Err(error) => {
                        if let Some(runtime) = machines
                            .get_mut(&machine_id)
                            .and_then(|machine| machine.endpoint_mut(&session))
                            .filter(|runtime| runtime.generation == generation)
                        {
                            runtime.state = MachineState::Attention(error);
                        }
                    }
                }
                dock.dirty = true;
            }
            ShellEvent::Input(message) => {
                if handle_dock_input(
                    &message,
                    &mut dock,
                    &active,
                    &mut candidate,
                    &mut pending_endpoint,
                    &mut machines,
                    &writer,
                    terminal,
                    &events_tx,
                )? {
                    continue;
                }
                if matches!(message, ClientMessage::Detach) {
                    exit = super::client::ClientExit::Detached;
                    running = false;
                    continue;
                }
                if let Err(error) = send_surface(&active, &message, &machines, &writer) {
                    if matches!(active, Endpoint::Local) {
                        return Err(error);
                    }
                    dock.warning = Some("active machine connection lost".to_string());
                    dock.dirty = true;
                    request_switch(
                        Endpoint::Local,
                        &mut candidate,
                        &machines,
                        dock.local_workspaces.len(),
                        dock.sidebars.as_ref(),
                        &writer,
                    )?;
                }
            }
            ShellEvent::LocalClosed(generation) => {
                if generation != local_generation {
                    continue;
                }
                if active == Endpoint::Local {
                    exit = super::client::ClientExit::ServerStopped;
                    break;
                }
                dock.warning = Some("local session is offline".into());
                dock.dirty = true;
            }
            ShellEvent::Local(generation, message) => {
                if generation != local_generation {
                    continue;
                }
                let paints_initial_local_frame = matches!(
                    &message,
                    ServerMessage::Frame(_) | ServerMessage::PreparedFrame { .. }
                );
                if let Some(reason) = handle_surface_message(
                    Endpoint::Local,
                    message,
                    &mut active,
                    &mut candidate,
                    &mut machines,
                    &writer,
                    terminal,
                    truecolor,
                    &mut dock,
                    &mut last_cursor,
                    &mut cursor_visible,
                )? {
                    exit = reason;
                    running = false;
                }
                initial_local_frame_painted |= paints_initial_local_frame;
            }
            ShellEvent::Link(event) => {
                if let Some(reason) = handle_link_event(
                    event,
                    &mut machines,
                    &mut active,
                    &mut candidate,
                    &writer,
                    terminal,
                    truecolor,
                    &mut dock,
                    &mut last_cursor,
                    &mut cursor_visible,
                )? {
                    exit = reason;
                    running = false;
                }
            }
            ShellEvent::MachineCreated(result) => match result {
                Ok(crate::machine::ProfileSetupOutcome::Ready(profile)) => {
                    if let Some(form) = dock.machine_form.as_mut() {
                        form.submitting = true;
                        form.error = None;
                        form.install_prompt = None;
                    }
                    pending_endpoint = Some(Endpoint::Remote {
                        machine_id: profile.id,
                        session: profile
                            .preferred_session
                            .unwrap_or_else(|| crate::session::DEFAULT_SESSION_NAME.to_string()),
                    });
                    dock.refresh_catalog = true;
                    dock.dirty = true;
                }
                Ok(crate::machine::ProfileSetupOutcome::ApprovalRequired(reason)) => {
                    if let Some(form) = dock.machine_form.as_mut() {
                        form.submitting = false;
                        form.error = None;
                        form.install_prompt = Some(reason);
                    } else {
                        dock.warning = Some(reason);
                    }
                    dock.dirty = true;
                }
                Err(error) => {
                    if let Some(form) = dock.machine_form.as_mut() {
                        form.submitting = false;
                        form.error = Some(error);
                        form.install_prompt = None;
                    } else {
                        dock.warning = Some(error);
                    }
                    dock.dirty = true;
                }
            },
            ShellEvent::MachineRemoved { machine_id, result } => match result {
                Ok(()) => {
                    close_selector(&mut dock, &active, &mut candidate, &machines, &writer)?;
                    pending_endpoint = pending_endpoint.filter(|endpoint| {
                        !matches!(endpoint, Endpoint::Remote { machine_id: pending_id, .. } if pending_id == &machine_id)
                    });
                    dock.refresh_catalog = true;
                    dock.dirty = true;
                }
                Err(error) => {
                    if let Some(MachinePopup::Confirm {
                        machine_id: pending_id,
                        removing,
                        error: message,
                        ..
                    }) = dock.machine_popup.as_mut()
                    {
                        if pending_id == &machine_id {
                            *removing = false;
                            *message = Some(error);
                            dock.dirty = true;
                        }
                    }
                }
            },
        }
    }

    for machine in machines.values_mut() {
        stop_link(&mut machine.runtime);
    }
    Ok(exit)
}

fn dock_row_count(machine_count: usize) -> u16 {
    u16::try_from(machine_count).unwrap_or(12).min(12)
}

fn projected_dock_row_count(
    machines: &HashMap<String, MachineRuntime>,
    endpoint: &Endpoint,
    local_workspace_count: usize,
) -> u16 {
    let rows = if matches!(endpoint, Endpoint::Local) {
        machines.len()
    } else {
        projected_local_workspace_count(machines.len(), local_workspace_count)
            .saturating_add(machines.len())
    };
    dock_row_count(rows)
}

fn projected_local_workspace_count(machine_count: usize, local_workspace_count: usize) -> usize {
    local_workspace_count.min(12usize.saturating_sub(machine_count))
}

fn dock_layout(
    endpoint: &Endpoint,
    machines: &HashMap<String, MachineRuntime>,
    _local_workspace_count: usize,
) -> ShellDockLayout {
    // Version-8 servers that predate complete shell ownership interpret this
    // message as an optional row reservation. Send a zero-row capability so
    // they fall back to their ordinary full surface instead of producing a
    // partially overpainted dock. Current servers use the message itself as
    // the capability and return the complete Workspaces geometry.
    ShellDockLayout {
        owns_workspaces: !machines.is_empty(),
        owns_session_chrome: matches!(endpoint, Endpoint::Remote { .. }),
        ..ShellDockLayout::default()
    }
}

fn sync_dock_rows(
    machines: &HashMap<String, MachineRuntime>,
    active: &Endpoint,
    local_writer: &Arc<Mutex<crate::ipc::transport::Conn>>,
    dock_rows: &mut u16,
    dock: &mut DockState,
) -> Result<()> {
    let rows = projected_dock_row_count(machines, &Endpoint::Local, 0);
    if rows == *dock_rows {
        return Ok(());
    }
    *dock_rows = rows;
    send_local(
        local_writer,
        &ClientMessage::ShellDockLayout(dock_layout(&Endpoint::Local, machines, 0)),
    )?;
    if !matches!(active, Endpoint::Local) {
        let _ = send_surface(
            active,
            &ClientMessage::ShellDockLayout(dock_layout(
                active,
                machines,
                dock.local_workspaces.len(),
            )),
            machines,
            local_writer,
        );
    }
    dock.dirty = true;
    Ok(())
}

fn start_local_reader(
    mut reader: BufReader<crate::ipc::transport::Conn>,
    events: Sender<ShellEvent>,
    generation: u64,
) -> Result<()> {
    std::thread::Builder::new()
        .name("client-surface".to_string())
        .stack_size(256 * 1024)
        .spawn(move || {
            while let Ok(message) = protocol::read_message::<_, ServerMessage>(&mut reader) {
                if events.send(ShellEvent::Local(generation, message)).is_err() {
                    break;
                }
            }
            let _ = events.send(ShellEvent::LocalClosed(generation));
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

fn start_due_links(
    machines: &mut HashMap<String, MachineRuntime>,
    events: &Sender<ShellEvent>,
    cols: u16,
    rows: u16,
) {
    let now = Instant::now();
    let mut starting = machines
        .values()
        .map(|machine| &machine.runtime)
        .filter(|endpoint| matches!(endpoint.state, MachineState::Connecting { .. }))
        .count();
    for machine in machines.values_mut() {
        for (session, endpoint) in
            std::iter::once((&machine.selected_session, &mut machine.runtime))
        {
            if !matches!(endpoint.state, MachineState::Reconnecting { at } if at <= now) {
                continue;
            }
            if starting >= 2 {
                return;
            }
            starting += 1;
            endpoint.generation = next_connection_generation();
            let shell = events.clone();
            let profile = machine.profile.clone();
            let session = session.clone();
            let generation = endpoint.generation;
            endpoint.state = MachineState::Connecting {
                deadline: now + LINK_HANDSHAKE_TIMEOUT,
            };
            if let Err(error) = std::thread::Builder::new()
                .name("machine-connect".into())
                .stack_size(256 * 1024)
                .spawn(move || {
                    let messages = shell.clone();
                    let result =
                        link::start(&profile, &session, generation, cols, rows, move |event| {
                            messages.send(ShellEvent::Link(event)).is_ok()
                        })
                        .map_err(|error| error.to_string());
                    if let Err(error) = shell.send(ShellEvent::Started {
                        machine_id: profile.id,
                        session,
                        generation,
                        result,
                    }) {
                        if let ShellEvent::Started {
                            result: Ok(task), ..
                        } = error.0
                        {
                            task.control.close();
                        }
                    }
                })
            {
                endpoint.state = MachineState::Attention(error.to_string());
            }
        }
    }
}

fn next_connection_generation() -> u64 {
    static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

fn expire_connecting(machines: &mut HashMap<String, MachineRuntime>) {
    let now = Instant::now();
    for machine in machines.values_mut() {
        for (session, endpoint) in
            std::iter::once((&machine.selected_session, &mut machine.runtime))
        {
            if matches!(endpoint.state, MachineState::Connecting { deadline } if deadline <= now) {
                // Closing a link can enqueue its final disconnect or startup
                // result. Neither may revive an expired attempt.
                endpoint.generation = next_connection_generation();
                stop_link(endpoint);
                endpoint.handshake_failures = endpoint.handshake_failures.saturating_add(1);
                if endpoint.handshake_failures >= 3 {
                    endpoint.state = MachineState::Attention(
                        "connection timed out repeatedly; select the machine to retry".into(),
                    );
                    continue;
                }
                endpoint.state = MachineState::Reconnecting {
                    at: now
                        + reconnect_delay(
                            &format!("{}:{session}", machine.profile.id),
                            endpoint.generation,
                            endpoint.backoff,
                        ),
                };
                endpoint.backoff = (endpoint.backoff * 2).min(RECONNECT_MAX);
            }
        }
    }
}

fn service_endpoint_health(machines: &mut HashMap<String, MachineRuntime>) -> Vec<Endpoint> {
    let now = Instant::now();
    let mut unhealthy = Vec::new();
    for machine in machines.values_mut() {
        let machine_id = machine.profile.id.clone();
        let session = machine.selected_session.clone();
        let runtime = &mut machine.runtime;
        if !matches!(runtime.state, MachineState::Online) {
            continue;
        }
        let timed_out = runtime
            .health_probe
            .is_some_and(|(_, deadline)| deadline <= now);
        let mut send_failed = false;
        if timed_out {
            send_failed = true;
        } else if runtime.health_probe.is_none()
            && now.saturating_duration_since(runtime.last_evidence) >= LINK_HEALTH_IDLE
        {
            let nonce = next_connection_generation();
            match runtime
                .control
                .as_ref()
                .ok_or_else(|| anyhow!("remote session connection is unavailable"))
                .and_then(|control| control.send(&ClientMessage::HealthCheck { nonce }))
            {
                Ok(()) => runtime.health_probe = Some((nonce, now + LINK_HEALTH_TIMEOUT)),
                Err(_) => send_failed = true,
            }
        }
        if !send_failed {
            continue;
        }
        runtime.generation = next_connection_generation();
        stop_link(runtime);
        runtime.health_probe = None;
        runtime.state = MachineState::Reconnecting {
            at: now
                + reconnect_delay(
                    &format!("{machine_id}:{session}"),
                    runtime.generation,
                    runtime.backoff,
                ),
        };
        runtime.backoff = (runtime.backoff * 2).min(RECONNECT_MAX);
        unhealthy.push(Endpoint::Remote {
            machine_id,
            session,
        });
    }
    unhealthy
}

fn next_deadline(
    machines: &HashMap<String, MachineRuntime>,
    candidate: Option<&SurfaceCandidate>,
) -> Option<Instant> {
    let launch_capacity = machines
        .values()
        .map(|machine| &machine.runtime)
        .filter(|endpoint| matches!(endpoint.state, MachineState::Connecting { .. }))
        .count()
        < 2;
    machines
        .values()
        .map(|machine| &machine.runtime)
        .filter_map(|endpoint| match endpoint.state {
            MachineState::Connecting { deadline } => Some(deadline),
            MachineState::Reconnecting { at } if launch_capacity => Some(at),
            MachineState::Online => endpoint
                .health_probe
                .map(|(_, deadline)| deadline)
                .or_else(|| endpoint.last_evidence.checked_add(LINK_HEALTH_IDLE)),
            _ => None,
        })
        .chain(candidate.map(|candidate| candidate.deadline))
        .min()
}

fn expire_candidate(
    candidate: &mut Option<SurfaceCandidate>,
    machines: &HashMap<String, MachineRuntime>,
    local_writer: &Arc<Mutex<crate::ipc::transport::Conn>>,
) -> bool {
    if candidate
        .as_ref()
        .is_some_and(|candidate| candidate.deadline <= Instant::now())
    {
        if let Some(expired) = candidate.take() {
            let _ = send_interest(
                &expired.endpoint,
                SurfaceInterest::Suspended,
                machines,
                local_writer,
            );
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

fn transition_disconnected_endpoint(
    machine_id: &str,
    session: &str,
    endpoint: &mut SessionRuntime,
    machine_enabled: bool,
    reason: String,
    now: Instant,
) {
    let failed_during_handshake = matches!(endpoint.state, MachineState::Connecting { .. });
    if !machine_enabled {
        endpoint.state = MachineState::Disabled;
        return;
    }
    if reason != "SSH connection closed" {
        endpoint.state = MachineState::Attention(reason);
        return;
    }
    if failed_during_handshake {
        endpoint.handshake_failures = endpoint.handshake_failures.saturating_add(1);
        if endpoint.handshake_failures >= 3 {
            endpoint.state = MachineState::Attention(format!(
                "connection failed repeatedly; run `luvus machine status {machine_id}`"
            ));
            return;
        }
    }
    endpoint.state = MachineState::Reconnecting {
        at: now
            + reconnect_delay(
                &format!("{machine_id}:{session}"),
                endpoint.generation,
                endpoint.backoff,
            ),
    };
    endpoint.backoff = (endpoint.backoff * 2).min(RECONNECT_MAX);
}

#[allow(clippy::too_many_arguments)]
fn handle_link_event(
    event: LinkEvent,
    machines: &mut HashMap<String, MachineRuntime>,
    active: &mut Endpoint,
    candidate: &mut Option<SurfaceCandidate>,
    local_writer: &Arc<Mutex<crate::ipc::transport::Conn>>,
    terminal: &mut DefaultTerminal,
    truecolor: bool,
    dock: &mut DockState,
    last_cursor: &mut Option<(u16, u16)>,
    cursor_visible: &mut bool,
) -> Result<Option<super::client::ClientExit>> {
    match event {
        LinkEvent::Message {
            machine_id,
            session,
            generation,
            message,
        } => {
            let Some(endpoint_runtime) = machines
                .get_mut(&machine_id)
                .and_then(|machine| machine.endpoint_mut(&session))
            else {
                return Ok(None);
            };
            if endpoint_runtime.generation != generation {
                return Ok(None);
            }
            endpoint_runtime.last_evidence = Instant::now();
            endpoint_runtime.health_probe = None;
            if matches!(message, ServerMessage::ServerShutdown { .. }) {
                // An explicit server stop is not a network outage. Fence the
                // ensuing EOF and require a user action to start this session.
                stop_link(endpoint_runtime);
                endpoint_runtime.generation = next_connection_generation();
                endpoint_runtime.state = MachineState::Attention("session stopped".into());
            }
            let endpoint = Endpoint::Remote {
                machine_id,
                session,
            };
            if let Some(exit) = handle_surface_message(
                endpoint,
                message,
                active,
                candidate,
                machines,
                local_writer,
                terminal,
                truecolor,
                dock,
                last_cursor,
                cursor_visible,
            )? {
                return Ok(Some(exit));
            }
        }
        LinkEvent::Disconnected {
            machine_id,
            session,
            generation,
            reason,
        } => {
            let Some(endpoint_runtime) = machines
                .get_mut(&machine_id)
                .and_then(|machine| machine.endpoint_mut(&session))
            else {
                return Ok(None);
            };
            if endpoint_runtime.generation != generation {
                return Ok(None);
            }
            stop_link(endpoint_runtime);
            let machine_enabled = machines
                .get(&machine_id)
                .is_some_and(|machine| machine.profile.enabled);
            let endpoint_runtime = machines
                .get_mut(&machine_id)
                .and_then(|machine| machine.endpoint_mut(&session))
                .expect("remote endpoint remains present");
            transition_disconnected_endpoint(
                &machine_id,
                &session,
                endpoint_runtime,
                machine_enabled,
                reason,
                Instant::now(),
            );
            let disconnected = Endpoint::Remote {
                machine_id,
                session,
            };
            if candidate
                .as_ref()
                .is_some_and(|pending| same_selection(&pending.endpoint, &disconnected))
            {
                *candidate = None;
            }
            if same_selection(active, &disconnected) {
                prepare_local_failover(candidate, local_writer, terminal)?;
            }
            dock.dirty = true;
        }
    }
    Ok(None)
}

fn replace_candidate_with_local(candidate: &mut Option<SurfaceCandidate>) {
    candidate.take();
    *candidate = Some(SurfaceCandidate {
        ticket: next_connection_generation(),
        endpoint: Endpoint::Local,
        welcomed: true,
        ready: true,
        shell_dock: None,
        boot_id: None,
        deadline: Instant::now() + SURFACE_PREPARE_TIMEOUT,
    });
}

fn prepare_local_failover(
    candidate: &mut Option<SurfaceCandidate>,
    local_writer: &Arc<Mutex<crate::ipc::transport::Conn>>,
    terminal: &DefaultTerminal,
) -> Result<()> {
    replace_candidate_with_local(candidate);
    send_local(
        local_writer,
        &ClientMessage::SurfaceInterest(SurfaceInterest::Prepared),
    )?;
    if let Ok(size) = terminal.size() {
        send_local(
            local_writer,
            &ClientMessage::PrepareSurface {
                ticket: candidate.as_ref().expect("local failover candidate").ticket,
                cols: size.width,
                rows: size.height,
            },
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn handle_surface_message(
    endpoint: Endpoint,
    message: ServerMessage,
    active: &mut Endpoint,
    candidate: &mut Option<SurfaceCandidate>,
    machines: &mut HashMap<String, MachineRuntime>,
    local_writer: &Arc<Mutex<crate::ipc::transport::Conn>>,
    terminal: &mut DefaultTerminal,
    truecolor: bool,
    dock: &mut DockState,
    last_cursor: &mut Option<(u16, u16)>,
    cursor_visible: &mut bool,
) -> Result<Option<super::client::ClientExit>> {
    let (message, prepared) = match message {
        ServerMessage::PreparedFrame { ticket, frame } => {
            let size = terminal.size()?;
            let boot_id = match &endpoint {
                Endpoint::Local => None,
                Endpoint::Remote {
                    machine_id,
                    session,
                } => machines
                    .get(machine_id)
                    .and_then(|machine| machine.endpoint(session))
                    .and_then(|runtime| runtime.boot_id),
            };
            if !candidate.as_ref().is_some_and(|candidate| {
                candidate.endpoint == endpoint
                    && candidate.ticket == ticket
                    && candidate.boot_id == boot_id
                    && frame.width == size.width
                    && frame.height == size.height
            }) {
                return Ok(None);
            }
            (ServerMessage::Frame(frame), true)
        }
        message => (message, false),
    };
    match message {
        ServerMessage::Welcome { version, error } => {
            let is_candidate = candidate
                .as_ref()
                .is_some_and(|candidate| candidate.endpoint == endpoint);
            let error = error.or_else(|| {
                (version != PROTOCOL_VERSION).then(|| {
                    format!(
                        "remote display protocol {version} does not match local {PROTOCOL_VERSION}"
                    )
                })
            });
            if let Some(error) = error {
                if let Endpoint::Remote {
                    machine_id,
                    session,
                } = &endpoint
                {
                    if let Some(runtime) = machines
                        .get_mut(machine_id)
                        .and_then(|machine| machine.endpoint_mut(session))
                    {
                        runtime.state = MachineState::Attention(error.clone());
                    }
                    if is_candidate {
                        *candidate = None;
                        set_machine_form_error(dock, &error);
                        dock.warning = Some(error);
                        dock.dirty = true;
                    }
                    return Ok(None);
                }
                return Err(anyhow!(error));
            }
            if let Some(candidate) = candidate
                .as_mut()
                .filter(|candidate| candidate.endpoint == endpoint)
            {
                candidate.welcomed = true;
            }
            if let Endpoint::Remote {
                machine_id,
                session,
            } = &endpoint
            {
                if let Some(runtime) = machines
                    .get_mut(machine_id)
                    .and_then(|machine| machine.endpoint_mut(session))
                {
                    runtime.welcomed = true;
                }
            }
        }
        ServerMessage::Ready { .. } => {
            let is_candidate = candidate
                .as_ref()
                .is_some_and(|candidate| candidate.endpoint == endpoint);
            if is_candidate {
                if let Some(pending) = candidate.as_mut() {
                    pending.ready = true;
                }
            }
            if let Endpoint::Remote {
                machine_id,
                session,
            } = &endpoint
            {
                let result = (|| {
                    let layout = dock_layout(&endpoint, machines, dock.local_workspaces.len());
                    let runtime = machines
                        .get_mut(machine_id)
                        .and_then(|machine| machine.endpoint_mut(session))
                        .ok_or_else(|| anyhow!("remote session disappeared during negotiation"))?;
                    runtime.ready = runtime.welcomed;
                    let control = runtime
                        .control
                        .as_ref()
                        .ok_or_else(|| anyhow!("remote session connection closed"))?;
                    // The input reader already owns stdin. Machine endpoints
                    // have separate image IDs; advertise text-only until those
                    // namespaces are virtualized across surface switches.
                    control.send(&ClientMessage::TerminalProbe {
                        colors: None,
                        graphics: Some(false),
                        cell_size: None,
                    })?;
                    control.send(&super::client::cell_pixels_message())?;
                    control.send(&ClientMessage::ShellDockLayout(layout))?;
                    if let Some(state) = &dock.sidebars {
                        control.send(&ClientMessage::ShellSidebars(state.clone()))?;
                    }
                    if let Some(width) = dock.workspace_width {
                        control.send(&ClientMessage::ShellDockLayout(ShellDockLayout {
                            workspace_width: Some(width),
                            ..layout
                        }))?;
                    }
                    control.send(&ClientMessage::SurfaceInterest(if is_candidate {
                        SurfaceInterest::Prepared
                    } else {
                        SurfaceInterest::Suspended
                    }))
                })();
                if let Err(error) = result {
                    if let Some(runtime) = machines
                        .get_mut(machine_id)
                        .and_then(|machine| machine.endpoint_mut(session))
                    {
                        runtime.state = MachineState::Attention(error.to_string());
                    }
                    if is_candidate {
                        *candidate = None;
                        set_machine_form_error(dock, "Remote session negotiation failed.");
                        dock.warning = Some("remote session negotiation failed".to_string());
                    }
                }
                dock.dirty = true;
            }
        }
        ServerMessage::EndpointIdentity {
            boot_id,
            session: reported_session,
        } => {
            if let Endpoint::Remote {
                machine_id,
                session,
            } = &endpoint
            {
                let Some(runtime) = machines
                    .get_mut(machine_id)
                    .and_then(|machine| machine.endpoint_mut(session))
                else {
                    return Ok(None);
                };
                if &reported_session != session {
                    stop_link(runtime);
                    runtime.state = MachineState::Attention(format!(
                        "remote endpoint selected session `{reported_session}` instead of `{session}`"
                    ));
                } else {
                    runtime.boot_id = Some(boot_id);
                }
                dock.dirty = true;
            }
        }
        ServerMessage::ShellSidebars(mut state) => {
            let acknowledged_revision =
                dock.sidebar_sync
                    .as_ref()
                    .and_then(|(pending_endpoint, revision)| {
                        (same_selection(pending_endpoint, &endpoint) && state.revision >= *revision)
                            .then_some(*revision)
                    });
            if acknowledged_revision.is_some() {
                dock.sidebar_sync = None;
            }
            // Only the active surface can change preferences. Initial seeding
            // comes from Local; warm endpoints and late replies cannot reset it.
            if !machines.is_empty() && endpoint == *active {
                let accept = match &dock.sidebars {
                    None => endpoint == Endpoint::Local,
                    Some(current) => state.revision > current.revision,
                };
                if accept {
                    if dock.sidebars.is_none() {
                        state.revision = state.revision.saturating_add(1);
                    }
                    dock.sidebars = Some(state);
                    // The shell client owns this projection. Keep the newest
                    // revision locally and seed only the endpoint selected by
                    // the user; fanning every drag update out to every warm
                    // machine made remote sidebar resizing unnecessarily
                    // expensive and could fill SSH writer queues.
                }
                if acknowledged_revision
                    .is_some_and(|revision| has_newer_sidebar_revision(dock, revision))
                {
                    send_latest_sidebar_state(dock, active, machines, local_writer);
                }
            }
        }
        ServerMessage::ShellDock(rect) => {
            if !machines.is_empty() && endpoint == Endpoint::Local && dock.workspace_width.is_none()
            {
                if let Some(rect) = rect {
                    dock.workspace_width = Some(rect.width);
                    broadcast_workspace_width(dock, machines, local_writer)?;
                }
            }
            if endpoint == Endpoint::Local {
                dock.owner_session_slot = rect.and_then(|rect| {
                    rect.session_slot
                        .map(|slot| Rect::new(slot.x, slot.y, slot.width, slot.height))
                });
                dock.owner_session_button = rect.and_then(|rect| {
                    rect.session_button
                        .map(|button| Rect::new(button.x, button.y, button.width, button.height))
                });
                dock.owner_style = rect;
                if let Some(owner) = rect {
                    dock.form_theme = owner.chrome;
                }
            }
            if endpoint == *active {
                let geometry_acknowledged =
                    dock.sidebar_sync
                        .as_ref()
                        .is_some_and(|(pending_endpoint, _)| {
                            same_selection(pending_endpoint, &endpoint)
                                && rect.is_some_and(|rect| {
                                    dock.sidebars.as_ref().is_some_and(|state| {
                                        shell_rect_matches_sidebar_state(rect, state)
                                    })
                                })
                        });
                if geometry_acknowledged {
                    dock.sidebar_sync = None;
                }
                // Geometry and frame bytes describe one surface revision. Keep
                // the current dock intact until the matching frame arrives so
                // a high-latency endpoint cannot paint a wider Workspaces tree
                // over panes that are still using the previous boundary.
                dock.pending_rect = Some(rect);
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
        ServerMessage::OpenMachineCreate { theme, modal } if endpoint == *active => {
            dock.selector_open = false;
            dock.machine_popup = None;
            dock.machine_form = Some(dock.machine_draft.take().unwrap_or_default());
            dock.form_theme = theme;
            dock.machine_form_rect = Some(Rect::new(modal.x, modal.y, modal.width, modal.height));
            dock.machine_form_backdrop_pending = false;
            dock.warning = None;
            dock.dirty = true;
        }
        ServerMessage::ShellWorkspaces(mut workspaces) if endpoint == Endpoint::Local => {
            workspaces.truncate(256);
            if dock.local_workspaces != workspaces {
                dock.local_workspaces = workspaces;
                if !matches!(active, Endpoint::Local) {
                    let _ = send_surface(
                        active,
                        &ClientMessage::ShellDockLayout(dock_layout(
                            active,
                            machines,
                            dock.local_workspaces.len(),
                        )),
                        machines,
                        local_writer,
                    );
                }
                dock.dirty = true;
            }
        }
        ServerMessage::MachineCatalogChanged { revision } if endpoint == Endpoint::Local => {
            note_catalog_revision(dock, revision);
        }
        ServerMessage::ShellWorkspaces(mut workspaces) => {
            if let Endpoint::Remote {
                machine_id,
                session,
            } = &endpoint
            {
                if let Some(runtime) = machines
                    .get_mut(machine_id)
                    .and_then(|machine| machine.endpoint_mut(session))
                {
                    workspaces.truncate(256);
                    if runtime.workspaces != workspaces {
                        runtime.workspaces = workspaces;
                        dock.dirty = true;
                    }
                    if runtime.welcomed && runtime.ready && runtime.boot_id.is_some() {
                        runtime.state = MachineState::Online;
                        runtime.backoff = Duration::from_secs(1);
                        runtime.handshake_failures = 0;
                        runtime.last_evidence = Instant::now();
                        runtime.health_probe = None;
                        dock.dirty = true;
                    }
                }
            }
        }
        ServerMessage::Frame(frame) => {
            let is_candidate = candidate.as_ref().is_some_and(|candidate| {
                candidate.endpoint == endpoint && candidate.welcomed && candidate.ready && prepared
            });
            if (endpoint == *active
                && (!dock.overlay_open() || dock.machine_form_backdrop_pending)
                && candidate
                    .as_ref()
                    .is_none_or(|candidate| candidate.endpoint != endpoint))
                || is_candidate
            {
                if endpoint == *active {
                    apply_pending_shell_rect(dock);
                }
                if is_candidate {
                    let reported = candidate
                        .as_ref()
                        .and_then(|candidate| candidate.shell_dock)
                        .flatten();
                    dock.rect =
                        candidate_shell_rect(&endpoint, reported, dock.rect, dock.owner_style);
                }
                cache_dock_projection(dock, &frame, truecolor, endpoint == Endpoint::Local);
                super::client::sync_begin();
                super::client::paint(
                    terminal,
                    &super::client::frame_cells(
                        &frame,
                        super::client::HostTerminal {
                            truecolor,
                            graphics: dock.graphics && endpoint == Endpoint::Local,
                        },
                    ),
                    frame.cursor,
                    frame.cursor_visible,
                    true,
                    last_cursor,
                )?;
                super::client::sync_end();
                *cursor_visible = frame.cursor_visible;
                dock.machine_form_backdrop_pending = false;
                dock.dirty = true;
                if is_candidate {
                    let committed_local = matches!(endpoint, Endpoint::Local);
                    if dock.machine_form.is_some() {
                        // The source picker stays alive beneath the
                        // client-owned tab. Close it only after the target is
                        // proven ready so a failed switch preserves the form.
                        let _ = send_surface(
                            active,
                            &ClientMessage::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
                            machines,
                            local_writer,
                        );
                    }
                    commit_candidate(endpoint, active, candidate, machines, local_writer)?;
                    dock.sidebar_sync = None;
                    dock.pending_rect = None;
                    dock.selector_open = false;
                    dock.selector_rect = None;
                    dock.machine_form = None;
                    dock.machine_form_rect = None;
                    dock.machine_form_hits.clear();
                    dock.machine_form_backdrop_pending = false;
                    dock.warning = None;
                    if committed_local {
                        if let Some(click) = dock.pending_local_session_click.take() {
                            send_local(local_writer, &ClientMessage::Mouse(click))?;
                        }
                    } else {
                        dock.pending_local_session_click = None;
                    }
                }
            }
        }
        ServerMessage::FrameDiff(diff)
            if endpoint == *active
                && (!dock.overlay_open() || dock.machine_form_backdrop_pending) =>
        {
            apply_pending_shell_rect(dock);
            let dock_changed = dock.rect.is_some_and(|rect| {
                frame_diff_intersects_rect(
                    &diff,
                    Rect::new(rect.x, rect.y, rect.width, rect.height),
                )
            }) || dock
                .owner_session_slot
                .is_some_and(|rect| frame_diff_intersects_rect(&diff, rect))
                || dock
                    .owner_session_button
                    .is_some_and(|rect| frame_diff_intersects_rect(&diff, rect));
            super::client::sync_begin();
            super::client::paint(
                terminal,
                &super::client::diff_cells(
                    &diff,
                    super::client::HostTerminal {
                        truecolor,
                        graphics: dock.graphics && endpoint == Endpoint::Local,
                    },
                ),
                diff.cursor,
                diff.cursor_visible,
                false,
                last_cursor,
            )?;
            super::client::sync_end();
            *cursor_visible = diff.cursor_visible;
            dock.machine_form_backdrop_pending = false;
            // A native modal dims the complete server frame, including the
            // reserved client-owned Workspaces slot. Restore semantic rows
            // only when a diff actually touched that slot; ordinary pane
            // output keeps the fast sparse path.
            if dock_changed {
                dock.dirty = true;
            }
        }
        ServerMessage::Graphics(commands) if endpoint == Endpoint::Local && dock.graphics => {
            crate::emit_graphics(&commands);
        }
        ServerMessage::Notify(message) if endpoint == *active => crate::emit_notification(&message),
        ServerMessage::Sound(signal) if endpoint == *active => crate::emit_sound(signal),
        ServerMessage::Clipboard(text) if endpoint == *active => crate::emit_clipboard(&text),
        ServerMessage::OpenUrl(url) if endpoint == *active => crate::platform::open_url(&url),
        ServerMessage::SwitchSession { name } if endpoint == *active => {
            if let Some(exit) = owner_local_session_switch(&endpoint, name) {
                return Ok(Some(exit));
            }
        }
        ServerMessage::Detach if endpoint == *active => {
            return Ok(Some(super::client::ClientExit::Detached));
        }
        ServerMessage::ServerShutdown { .. } if endpoint == *active => match endpoint {
            Endpoint::Local => return Ok(Some(super::client::ClientExit::ServerStopped)),
            Endpoint::Remote { .. } => {
                // One remote session ending must not tear down the owner-local
                // TUI or sibling machine connections. Prepare the local
                // surface immediately; the ensuing link disconnect schedules
                // the ordinary bounded reconnect for this endpoint.
                prepare_local_failover(candidate, local_writer, terminal)?;
                dock.warning = Some("remote session stopped".to_string());
                dock.dirty = true;
            }
        },
        _ => {}
    }
    Ok(None)
}

fn active_shell_rect(reported: Option<ShellDockRect>) -> Option<ShellDockRect> {
    // An explicit ShellDock control message is authoritative for the active
    // viewport. In particular, `None` means compact/mobile layout has no dock
    // slot; retaining desktop geometry would paint stale rows through a modal.
    reported
}

fn apply_pending_shell_rect(dock: &mut DockState) {
    let Some(reported) = dock.pending_rect.take() else {
        return;
    };
    let rect = active_shell_rect(reported);
    if let Some(rect) = rect.filter(|_| dock.owner_style.is_none()) {
        dock.form_theme = rect.chrome;
    }
    dock.rect = rect;
    if let Some(rect) = rect {
        if dock.machine_form.is_some() && rect.workspace_modal {
            dock.machine_form_rect = rect
                .overlay
                .map(|overlay| Rect::new(overlay.x, overlay.y, overlay.width, overlay.height));
        }
        if rect.workspace_focused && !rect.workspace_modal && !dock.focus_seen {
            dock.navigation = Some(0);
            dock.navigation_reveal = true;
        }
        dock.focus_seen = rect.workspace_focused && !rect.workspace_modal;
    }
    dock.dirty = true;
}

fn shell_rect_matches_sidebar_state(rect: ShellDockRect, state: &protocol::ShellSidebars) -> bool {
    let Some(resize) = rect.resize else {
        return false;
    };
    let side = if resize.left {
        &state.layout.left
    } else {
        &state.layout.right
    };
    side.docks.iter().any(|dock| dock == "workspaces") && side.width == rect.width
}

fn candidate_shell_rect(
    endpoint: &Endpoint,
    reported: Option<ShellDockRect>,
    current: Option<ShellDockRect>,
    owner: Option<ShellDockRect>,
) -> Option<ShellDockRect> {
    if matches!(endpoint, Endpoint::Local) {
        // A candidate frame can overtake its ShellDock control message. Keep
        // the last owner-local geometry until that authoritative message is
        // processed instead of flashing or clearing the entire navigation
        // shell during a remote-to-local switch.
        reported.or(owner).or(current)
    } else {
        reported.or(current).or(owner)
    }
}

fn cache_dock_projection(
    dock: &mut DockState,
    frame: &protocol::FrameData,
    truecolor: bool,
    owner_local: bool,
) {
    let (x, y) = dock.rect.map_or((0, 0), |rect| (rect.x, rect.y));
    let index = usize::from(y)
        .saturating_mul(usize::from(frame.width))
        .saturating_add(usize::from(x));
    if let Some(cell) = frame.cells.get(index) {
        dock.base = super::client::make_cell(
            &cell.symbol,
            cell.fg,
            cell.bg,
            cell.mods,
            super::client::HostTerminal {
                truecolor,
                graphics: false,
            },
        );
        if owner_local {
            dock.owner_base = Some(dock.base.clone());
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_dock_input(
    message: &ClientMessage,
    dock: &mut DockState,
    active: &Endpoint,
    candidate: &mut Option<SurfaceCandidate>,
    pending_endpoint: &mut Option<Endpoint>,
    machines: &mut HashMap<String, MachineRuntime>,
    local_writer: &Arc<Mutex<crate::ipc::transport::Conn>>,
    terminal: &mut DefaultTerminal,
    events: &Sender<ShellEvent>,
) -> Result<bool> {
    if dock.sidebars.is_some() && !dock.overlay_open() {
        if let ClientMessage::Mouse(mouse) = message {
            if dock.width_drag.is_some()
                && matches!(
                    mouse.kind,
                    MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
                )
            {
                if matches!(active, Endpoint::Remote { .. }) {
                    if update_client_sidebar_drag(dock, mouse.column) {
                        send_latest_sidebar_state(dock, active, machines, local_writer);
                    }
                    if mouse.kind == MouseEventKind::Up(MouseButton::Left) {
                        dock.width_drag = None;
                    }
                    return Ok(true);
                }
                if mouse.kind == MouseEventKind::Up(MouseButton::Left) {
                    dock.width_drag = None;
                }
                return Ok(false);
            }
            if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                if let Some(seam) = dock
                    .rect
                    .filter(|rect| !rect.workspace_modal)
                    .and_then(|rect| rect.resize)
                    .filter(|seam| {
                        mouse.column == seam.column
                            && mouse.row >= seam.top
                            && mouse.row < seam.bottom
                    })
                {
                    // Remote chrome is client-owned: claim its resize gesture
                    // locally so the visible seam follows the pointer without
                    // waiting for an SSH round trip. Local keeps the native
                    // server path that was already immediate.
                    dock.width_drag = Some(seam);
                    return Ok(matches!(active, Endpoint::Remote { .. }));
                }
            }
        }
    }
    if !machines.is_empty() && dock.sidebars.is_none() && !dock.overlay_open() {
        if let ClientMessage::Mouse(mouse) = message {
            if let Some(drag) = dock.width_drag {
                if matches!(
                    mouse.kind,
                    MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
                ) {
                    let width = resized_workspace_width(drag, mouse.column);
                    if dock.workspace_width != Some(width) {
                        dock.workspace_width = Some(width);
                        broadcast_workspace_width(dock, machines, local_writer)?;
                    }
                    if mouse.kind == MouseEventKind::Up(MouseButton::Left) {
                        dock.width_drag = None;
                    }
                    return Ok(true);
                }
            }
            if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                if let Some(resize) = dock
                    .rect
                    .filter(|rect| !rect.workspace_modal)
                    .and_then(|rect| rect.resize)
                {
                    if mouse.column == resize.column
                        && mouse.row >= resize.top
                        && mouse.row < resize.bottom
                    {
                        dock.width_drag = Some(resize);
                        return Ok(true);
                    }
                }
            }
        }
    }
    if dock.machine_popup.is_some() {
        if let Some(MachinePopup::Menu {
            machine_id,
            rect,
            selected,
            ..
        }) = dock.machine_popup.as_mut()
        {
            let mut activate_connection = false;
            match message {
                ClientMessage::Key(key) => {
                    if move_machine_popup_selection(selected, &key.code) {
                        dock.dirty = true;
                    } else if key.code == KeyCode::Enter && *selected == 0 {
                        activate_connection = true;
                    }
                }
                ClientMessage::Mouse(mouse)
                    if mouse.kind == MouseEventKind::Down(MouseButton::Left) =>
                {
                    activate_connection = rect.is_some_and(|rect| {
                        rect.contains((mouse.column, mouse.row).into()) && mouse.row == rect.y + 1
                    });
                }
                _ => {}
            }
            if activate_connection {
                let id = machine_id.clone();
                close_selector(dock, active, candidate, machines, local_writer)?;
                if let Some(machine) = machines.get_mut(&id) {
                    if machine_can_disconnect(&machine.runtime.state) {
                        disconnect_machine_runtime(&mut machine.runtime);
                        if dock.pending_workspace_menu.as_ref().is_some_and(|(endpoint, ..)| matches!(endpoint, Endpoint::Remote { machine_id, .. } if machine_id == &id)) {
                            dock.pending_workspace_menu = None;
                        }
                        if pending_endpoint.as_ref().is_some_and(|endpoint| matches!(endpoint, Endpoint::Remote { machine_id, .. } if machine_id == &id)) {
                            *pending_endpoint = None;
                        }
                        if candidate.as_ref().is_some_and(|surface| matches!(&surface.endpoint, Endpoint::Remote { machine_id, .. } if machine_id == &id)) {
                            *candidate = None;
                        }
                        if matches!(active, Endpoint::Remote { machine_id, .. } if machine_id == &id)
                        {
                            prepare_local_failover(candidate, local_writer, terminal)?;
                        }
                    } else {
                        let events = events.clone();
                        std::thread::Builder::new()
                            .name("machine-enable".into())
                            .stack_size(512 * 1024)
                            .spawn(move || {
                                let result = crate::machine::enable_profile(&id, false)
                                    .map_err(|error| error.to_string());
                                let _ = events.send(ShellEvent::MachineCreated(result));
                            })?;
                    }
                }
                dock.dirty = true;
                return Ok(true);
            }
        }
        match message {
            ClientMessage::Key(key) => match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('n')
                    if !machine_popup_removing(dock) =>
                {
                    close_selector(dock, active, candidate, machines, local_writer)?
                }
                KeyCode::Enter => {
                    advance_machine_popup(dock, events)?;
                }
                KeyCode::Char('y')
                    if matches!(dock.machine_popup, Some(MachinePopup::Confirm { .. })) =>
                {
                    advance_machine_popup(dock, events)?;
                }
                _ => {}
            },
            ClientMessage::Mouse(mouse)
                if mouse.kind == MouseEventKind::Down(MouseButton::Left) =>
            {
                let point = (mouse.column, mouse.row).into();
                let (inside, action, cancel, can_close) = match dock.machine_popup.as_ref() {
                    Some(MachinePopup::Menu {
                        rect, remove_rect, ..
                    }) => (
                        rect.is_some_and(|rect| rect.contains(point)),
                        remove_rect.is_some_and(|rect| rect.contains(point)),
                        false,
                        true,
                    ),
                    Some(MachinePopup::Confirm {
                        rect,
                        confirm_rect,
                        cancel_rect,
                        removing,
                        ..
                    }) => (
                        rect.is_some_and(|rect| rect.contains(point)),
                        !removing && confirm_rect.is_some_and(|rect| rect.contains(point)),
                        !removing && cancel_rect.is_some_and(|rect| rect.contains(point)),
                        !removing,
                    ),
                    None => (false, false, false, false),
                };
                if action {
                    advance_machine_popup(dock, events)?;
                } else if can_close && (cancel || !inside) {
                    close_selector(dock, active, candidate, machines, local_writer)?;
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
    if dock.machine_form.is_some() {
        if machine_form_awaiting_approval(dock) {
            match message {
                ClientMessage::Key(key) => match install_prompt_choice(&key.code) {
                    Some(true) => {
                        clear_install_prompt(dock);
                        activate_machine_form(dock, events, pending_endpoint, machines, true)?;
                    }
                    Some(false) => clear_install_prompt(dock),
                    None => {}
                },
                ClientMessage::Mouse(mouse)
                    if mouse.kind == MouseEventKind::Down(MouseButton::Left) =>
                {
                    let hit = dock
                        .machine_form_hits
                        .iter()
                        .find(|(_, rect)| rect.contains((mouse.column, mouse.row).into()))
                        .map(|(hit, _)| *hit);
                    match hit {
                        Some(MachineFormHit::ApproveInstall) => {
                            clear_install_prompt(dock);
                            activate_machine_form(dock, events, pending_endpoint, machines, true)?;
                        }
                        Some(MachineFormHit::DeclineInstall) | None => clear_install_prompt(dock),
                        _ => {}
                    }
                }
                ClientMessage::Resize { .. } => {
                    dock.machine_form_rect = None;
                    dock.machine_form_backdrop_pending = true;
                    dock.dirty = true;
                }
                _ => {}
            }
            if dock.dirty {
                paint_dock(terminal, dock, machines, active, &mut None, false)?;
            }
            return Ok(true);
        }
        match message {
            ClientMessage::Key(key) => match key.code {
                KeyCode::Esc if !machine_form_submitting(dock) => {
                    cancel_machine_form(dock, active, candidate, machines, local_writer)?
                }
                KeyCode::Tab | KeyCode::BackTab if !machine_form_submitting(dock) => {
                    switch_to_workspace_tab(dock, active, candidate, machines, local_writer)?;
                }
                KeyCode::Down if !machine_form_submitting(dock) => {
                    move_machine_form_cursor(dock, 1);
                }
                KeyCode::Up if !machine_form_submitting(dock) => {
                    move_machine_form_cursor(dock, -1);
                }
                KeyCode::Enter if !machine_form_submitting(dock) => {
                    activate_machine_form(dock, events, pending_endpoint, machines, false)?;
                }
                KeyCode::Left if !machine_form_submitting(dock) => {
                    move_machine_form_edit_cursor(dock, -1, key.modifiers);
                }
                KeyCode::Right if !machine_form_submitting(dock) => {
                    move_machine_form_edit_cursor(dock, 1, key.modifiers);
                }
                KeyCode::Home if !machine_form_submitting(dock) => {
                    move_machine_form_edit_cursor_to_edge(dock, false);
                }
                KeyCode::End if !machine_form_submitting(dock) => {
                    move_machine_form_edit_cursor_to_edge(dock, true);
                }
                KeyCode::Backspace if !machine_form_submitting(dock) => {
                    delete_machine_form_text(dock, false, key.modifiers);
                }
                KeyCode::Delete if !machine_form_submitting(dock) => {
                    delete_machine_form_text(dock, true, key.modifiers);
                }
                KeyCode::Char(character)
                    if !machine_form_submitting(dock)
                        && !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    append_machine_form_text(dock, &character.to_string());
                }
                _ => {}
            },
            ClientMessage::Paste(text) if !machine_form_submitting(dock) => {
                append_machine_form_text(dock, text);
            }
            ClientMessage::Mouse(mouse)
                if mouse.kind == MouseEventKind::Down(MouseButton::Left) =>
            {
                let hit = dock
                    .machine_form_hits
                    .iter()
                    .find(|(_, rect)| rect.contains((mouse.column, mouse.row).into()))
                    .map(|(hit, _)| *hit);
                match hit {
                    Some(MachineFormHit::OpenWorkspaceTab) if !machine_form_submitting(dock) => {
                        switch_to_workspace_tab(dock, active, candidate, machines, local_writer)?;
                    }
                    Some(MachineFormHit::RemoteMachineTab | MachineFormHit::Modal) => {}
                    Some(MachineFormHit::Field(field)) if !machine_form_submitting(dock) => {
                        if let Some(form) = dock.machine_form.as_mut() {
                            form.cursor = field.min(form.fields.len() - 1);
                            form.selected_saved = None;
                            dock.dirty = true;
                        }
                    }
                    Some(MachineFormHit::Submit) if !machine_form_submitting(dock) => {
                        activate_machine_form(dock, events, pending_endpoint, machines, false)?;
                    }
                    Some(MachineFormHit::Saved(index)) if !machine_form_submitting(dock) => {
                        if let Some(id) = dock.saved_ids.get(index).cloned() {
                            if let Some(form) = dock.machine_form.as_mut() {
                                form.cursor = form.fields.len() + index;
                                form.selected_saved = Some(id.clone());
                                form.error = None;
                            }
                            dock.dirty = true;
                            // Disabled entries are selected first; Enter is the
                            // explicit Enable action, never an accidental click.
                            if machines
                                .get(&id)
                                .is_some_and(|machine| machine.profile.enabled)
                            {
                                activate_machine_form(
                                    dock,
                                    events,
                                    pending_endpoint,
                                    machines,
                                    false,
                                )?;
                            }
                        }
                    }
                    Some(MachineFormHit::Cancel) if !machine_form_submitting(dock) => {
                        cancel_machine_form(dock, active, candidate, machines, local_writer)?;
                    }
                    Some(_) => {}
                    None if !machine_form_submitting(dock) => {
                        cancel_machine_form(dock, active, candidate, machines, local_writer)?
                    }
                    None => {}
                }
            }
            ClientMessage::Resize { .. } => {
                dock.machine_form_rect = None;
                dock.machine_form_backdrop_pending = true;
                // Wait for the selected server's resized native picker frame
                // to clear the old client-owned form before painting the form
                // again at its new rectangle.
                dock.dirty = false;
                return Ok(false);
            }
            _ => {}
        }
        if dock.dirty {
            paint_dock(terminal, dock, machines, active, &mut None, false)?;
        }
        return Ok(true);
    }
    if dock.selector_open {
        match message {
            ClientMessage::Key(key) => {
                let endpoints = selector_endpoints(machines);
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        close_selector(dock, active, candidate, machines, local_writer)?;
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
                            if !same_selection(endpoint, active)
                                && !request_user_switch(
                                    endpoint.clone(),
                                    dock,
                                    active,
                                    candidate,
                                    pending_endpoint,
                                    machines,
                                    local_writer,
                                )?
                            {
                                paint_dock(terminal, dock, machines, active, &mut None, false)?;
                                return Ok(true);
                            }
                        }
                        close_selector(dock, active, candidate, machines, local_writer)?;
                    }
                    _ => {}
                }
            }
            ClientMessage::Mouse(mouse)
                if mouse.kind == MouseEventKind::Down(MouseButton::Left) =>
            {
                let hit = dock.hits.iter().find_map(|(hit, rect)| {
                    rect.contains((mouse.column, mouse.row).into())
                        .then(|| match hit {
                            DockHit::Endpoint(endpoint) | DockHit::RemoteWorkspace(endpoint, _) => {
                                Some(endpoint.clone())
                            }
                            DockHit::AddWorkspace
                            | DockHit::LocalWorkspace(_)
                            | DockHit::Machine(_) => None,
                        })
                        .flatten()
                });
                if let Some(endpoint) = hit {
                    if !same_selection(&endpoint, active)
                        && !request_user_switch(
                            endpoint,
                            dock,
                            active,
                            candidate,
                            pending_endpoint,
                            machines,
                            local_writer,
                        )?
                    {
                        paint_dock(terminal, dock, machines, active, &mut None, false)?;
                        return Ok(true);
                    }
                    close_selector(dock, active, candidate, machines, local_writer)?;
                } else if !dock
                    .selector_rect
                    .is_some_and(|rect| rect.contains((mouse.column, mouse.row).into()))
                {
                    close_selector(dock, active, candidate, machines, local_writer)?;
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
    if let ClientMessage::Mouse(mouse) = message {
        let over_local_session = owner_session_projection(dock)
            .is_some_and(|(_, rect, _)| rect.contains((mouse.column, mouse.row).into()));
        if mouse.kind == MouseEventKind::Moved && dock.owner_session_hovered != over_local_session {
            dock.owner_session_hovered = over_local_session;
            dock.dirty = true;
            if !matches!(active, Endpoint::Local) {
                paint_dock(terminal, dock, machines, active, &mut None, false)?;
            }
        }
        if over_local_session
            && mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && !matches!(active, Endpoint::Local)
        {
            // The named-session selector belongs to the owner-local server.
            // First prepare that complete surface, then replay this exact click
            // after it is visible. A machine endpoint never receives or
            // interprets the session control.
            dock.pending_local_session_click = Some(*mouse);
            request_user_switch(
                Endpoint::Local,
                dock,
                active,
                candidate,
                pending_endpoint,
                machines,
                local_writer,
            )?;
            return Ok(true);
        }
    }
    if machines.is_empty() {
        return Ok(false);
    }
    if let (Some(index), ClientMessage::Key(key), Some(rect)) =
        (dock.navigation, message, dock.rect)
    {
        if !key.modifiers.difference(KeyModifiers::SHIFT).is_empty() {
            // Let the configured prefix and direct chords reach native input.
            dock.navigation = None;
            dock.focus_seen = false;
            return Ok(false);
        }
        if !rect.workspace_modal && key.modifiers.difference(KeyModifiers::SHIFT).is_empty() {
            let rows = visible_dock_rows(machines, active, dock);
            let end = rows.len().saturating_sub(1);
            let next = match key.code {
                KeyCode::Up | KeyCode::Char('k') => Some(index.saturating_sub(1)),
                KeyCode::Down | KeyCode::Char('j') => Some((index + 1).min(end)),
                KeyCode::Home | KeyCode::Char('g') => Some(0),
                KeyCode::End | KeyCode::Char('G') => Some(end),
                KeyCode::PageUp => {
                    Some(index.saturating_sub(usize::from(rect.height.saturating_sub(1)).max(1)))
                }
                KeyCode::PageDown => Some(
                    index
                        .saturating_add(usize::from(rect.height.saturating_sub(1)).max(1))
                        .min(end),
                ),
                _ => None,
            };
            if let Some(next) = next {
                dock.navigation = Some(next);
                dock.navigation_reveal = true;
                dock.dirty = true;
                paint_dock(terminal, dock, machines, active, &mut None, false)?;
                return Ok(true);
            }
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                dock.navigation = None;
                dock.dirty = true;
                return Ok(false);
            }
            if matches!(
                key.code,
                KeyCode::Enter | KeyCode::Char('a') | KeyCode::Left | KeyCode::Right
            ) {
                let Some(target) = rows.get(index.min(end)).map(dock_row_hit) else {
                    return Ok(true);
                };
                if matches!(key.code, KeyCode::Left | KeyCode::Right) {
                    let collapsed = key.code == KeyCode::Left;
                    match &target {
                        DockHit::Endpoint(Endpoint::Local) => dock.local_collapsed = collapsed,
                        DockHit::Machine(id) => {
                            if collapsed {
                                dock.collapsed_machines.insert(id.clone());
                            } else {
                                dock.collapsed_machines.remove(id);
                            }
                        }
                        _ => return Ok(true),
                    }
                    dock.dirty = true;
                    paint_dock(terminal, dock, machines, active, &mut None, false)?;
                    return Ok(true);
                }
                let Some((_, area)) = dock.hits.iter().find(|(hit, _)| hit == &target) else {
                    return Ok(true);
                };
                let button = if key.code == KeyCode::Char('a') {
                    MouseButton::Right
                } else {
                    MouseButton::Left
                };
                let col = area.x
                    + if matches!(key.code, KeyCode::Left | KeyCode::Right) {
                        2
                    } else {
                        5
                    };
                let row = area.y;
                if key.code == KeyCode::Enter {
                    dock.navigation = None;
                    send_surface(
                        active,
                        &ClientMessage::Key(ratatui::crossterm::event::KeyEvent::new(
                            KeyCode::Esc,
                            KeyModifiers::NONE,
                        )),
                        machines,
                        local_writer,
                    )?;
                }
                return handle_dock_input(
                    &ClientMessage::Mouse(ratatui::crossterm::event::MouseEvent {
                        kind: MouseEventKind::Down(button),
                        column: col,
                        row,
                        modifiers: KeyModifiers::NONE,
                    }),
                    dock,
                    active,
                    candidate,
                    pending_endpoint,
                    machines,
                    local_writer,
                    terminal,
                    events,
                );
            }
            return Ok(true);
        }
    }
    // Navigation stays server-owned. Only translate the native menu anchor
    // into the projected row's position; never interpret keys in a modal.
    if let ClientMessage::Key(key) = message {
        if key.code == KeyCode::Char('a')
            && key.modifiers.is_empty()
            && dock.rect.is_some_and(|rect| !rect.workspace_modal)
        {
            for (hit, area) in &dock.hits {
                if let Some((endpoint, workspace_id)) = workspace_hit_target(hit, dock, machines) {
                    let workspaces = match &endpoint {
                        Endpoint::Local => Some(&dock.local_workspaces),
                        Endpoint::Remote {
                            machine_id,
                            session,
                        } => machines
                            .get(machine_id)
                            .and_then(|machine| machine.endpoint(session))
                            .map(|runtime| &runtime.workspaces),
                    };
                    if same_selection(&endpoint, active)
                        && workspaces.is_some_and(|rows| {
                            rows.iter().any(|ws| ws.id == workspace_id && ws.selected)
                        })
                    {
                        send_surface(
                            active,
                            &ClientMessage::ShellWorkspaceMenu {
                                workspace_id,
                                col: area.x + 2,
                                row: area.y,
                            },
                            machines,
                            local_writer,
                        )?;
                        return Ok(true);
                    }
                }
            }
        }
    }
    let ClientMessage::Mouse(mouse) = message else {
        return Ok(false);
    };
    let Some(rect) = dock.rect else {
        return Ok(false);
    };
    if rect.workspace_modal {
        return Ok(false);
    }
    let point = (mouse.column, mouse.row).into();
    let dock_area = Rect::new(rect.x, rect.y, rect.width, rect.height);
    if matches!(mouse.kind, MouseEventKind::Moved) {
        let hover = dock
            .hits
            .iter()
            .find(|(_, rect)| rect.contains(point))
            .map(|(hit, _)| hit.clone());
        if dock.hover != hover {
            dock.hover = hover;
            dock.dirty = true;
        }
    }
    // Session, Menu and collapse buttons remain native server controls.
    if !dock_area.contains(point) {
        return Ok(false);
    }
    if dock_area.contains(point)
        && matches!(
            mouse.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        )
    {
        let rows = visible_dock_rows(machines, active, dock);
        let max = dock_max_scroll(&rows, rect.height.saturating_sub(1), rect.show_paths);
        dock.scroll = if mouse.kind == MouseEventKind::ScrollUp {
            dock.scroll.saturating_sub(3)
        } else {
            dock.scroll.saturating_add(3).min(max)
        };
        dock.dirty = true;
        return Ok(true);
    }
    // A server-owned popup may overlap the appended endpoint rows. Its pixels
    // and input remain authoritative; only unobscured endpoint cells belong to
    // this client-side hit layer.
    if rect
        .overlay
        .is_some_and(|overlay| shell_block_contains(overlay, mouse.column, mouse.row))
    {
        return Ok(false);
    }
    let hit = dock
        .hits
        .iter()
        .find(|(_, hit)| hit.contains((mouse.column, mouse.row).into()))
        .cloned();
    if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
        let release_server_focus =
            claim_pointer_navigation(dock, hit.as_ref().map(|(target, _)| target));
        if release_server_focus {
            // Match the native Workspaces click path: pointer activation ends
            // keyboard-list ownership before it changes workspace or endpoint.
            // Without this, the old row kept its full selection background
            // while a remote workspace was already active.
            send_surface(
                active,
                &ClientMessage::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
                machines,
                local_writer,
            )?;
        }
    }
    if mouse.kind == MouseEventKind::Down(MouseButton::Right) {
        let target = hit
            .as_ref()
            .and_then(|(hit, _)| workspace_hit_target(hit, dock, machines));
        if let Some((endpoint, workspace_id)) = target {
            let online = match &endpoint {
                Endpoint::Local => true,
                Endpoint::Remote {
                    machine_id,
                    session,
                } => machines
                    .get(machine_id)
                    .and_then(|machine| machine.endpoint(session))
                    .is_some_and(|runtime| matches!(runtime.state, MachineState::Online)),
            };
            if !online {
                return Ok(true);
            }
            if endpoint == *active {
                send_surface(
                    &endpoint,
                    &ClientMessage::ShellWorkspaceMenu {
                        workspace_id,
                        col: mouse.column,
                        row: mouse.row,
                    },
                    machines,
                    local_writer,
                )?;
            } else {
                dock.pending_workspace_menu =
                    Some((endpoint.clone(), workspace_id, mouse.column, mouse.row));
                request_user_switch(
                    endpoint,
                    dock,
                    active,
                    candidate,
                    pending_endpoint,
                    machines,
                    local_writer,
                )?;
            }
            return Ok(true);
        }
        let machine_id = hit.as_ref().and_then(|(hit, _)| match hit {
            DockHit::AddWorkspace | DockHit::LocalWorkspace(_) => None,
            DockHit::Machine(machine_id)
            | DockHit::RemoteWorkspace(Endpoint::Remote { machine_id, .. }, _)
            | DockHit::Endpoint(Endpoint::Remote { machine_id, .. }) => Some(machine_id.clone()),
            DockHit::Endpoint(Endpoint::Local) | DockHit::RemoteWorkspace(Endpoint::Local, _) => {
                None
            }
        });
        if let Some(machine_id) = machine_id {
            if let Some(machine) = machines.get(&machine_id) {
                dock.machine_popup = Some(MachinePopup::Menu {
                    machine_id,
                    label: machine.profile.label.clone(),
                    anchor: (mouse.column, mouse.row),
                    rect: None,
                    remove_rect: None,
                    selected: 0,
                });
                dock.dirty = true;
                return Ok(true);
            }
        }
    } else if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
        // Only the disclosure gutter folds a group. Its label retains the
        // existing endpoint-switch action.
        if mouse.column < rect.x.saturating_add(4) {
            match hit.as_ref().map(|(hit, _)| hit) {
                Some(DockHit::Endpoint(Endpoint::Local)) => {
                    dock.local_collapsed = !dock.local_collapsed;
                    dock.dirty = true;
                    return Ok(true);
                }
                Some(DockHit::Machine(id)) => {
                    if !dock.collapsed_machines.remove(id) {
                        dock.collapsed_machines.insert(id.clone());
                    }
                    dock.dirty = true;
                    return Ok(true);
                }
                _ => {}
            }
        }
        if hit
            .as_ref()
            .is_some_and(|(hit, _)| matches!(hit, DockHit::AddWorkspace))
        {
            send_surface(
                active,
                &ClientMessage::OpenWorkspacePicker,
                machines,
                local_writer,
            )?;
            return Ok(true);
        }
        if let Some((DockHit::RemoteWorkspace(endpoint, index), _)) = hit.as_ref() {
            let online = match endpoint {
                Endpoint::Remote {
                    machine_id,
                    session,
                } => machines
                    .get(machine_id)
                    .and_then(|machine| machine.endpoint(session))
                    .is_some_and(|runtime| matches!(runtime.state, MachineState::Online)),
                Endpoint::Local => false,
            };
            if online {
                let Some((_, workspace_id)) = workspace_hit_target(
                    &DockHit::RemoteWorkspace(endpoint.clone(), *index),
                    dock,
                    machines,
                ) else {
                    return Ok(true);
                };
                send_surface(
                    endpoint,
                    &ClientMessage::ShellWorkspaceFocus { workspace_id },
                    machines,
                    local_writer,
                )?;
                request_user_switch(
                    endpoint.clone(),
                    dock,
                    active,
                    candidate,
                    pending_endpoint,
                    machines,
                    local_writer,
                )?;
            }
            return Ok(true);
        }
        let local_workspace = hit.as_ref().and_then(|(hit, _)| match hit {
            DockHit::LocalWorkspace(index) => Some(*index),
            _ => None,
        });
        if let Some(index) = local_workspace {
            let Some((_, workspace_id)) =
                workspace_hit_target(&DockHit::LocalWorkspace(index), dock, machines)
            else {
                return Ok(true);
            };
            send_local(
                local_writer,
                &ClientMessage::ShellWorkspaceFocus { workspace_id },
            )?;
            if !matches!(active, Endpoint::Local) {
                request_user_switch(
                    Endpoint::Local,
                    dock,
                    active,
                    candidate,
                    pending_endpoint,
                    machines,
                    local_writer,
                )?;
            }
            return Ok(true);
        }
        let endpoint = hit.and_then(|(hit, _)| match hit {
            DockHit::AddWorkspace | DockHit::LocalWorkspace(_) => None,
            DockHit::Machine(machine_id) => {
                machines.get(&machine_id).map(|machine| Endpoint::Remote {
                    machine_id,
                    session: machine.session(),
                })
            }
            DockHit::Endpoint(endpoint) | DockHit::RemoteWorkspace(endpoint, _) => Some(endpoint),
        });
        if let Some(endpoint) = endpoint {
            if !same_selection(&endpoint, active) {
                request_user_switch(
                    endpoint,
                    dock,
                    active,
                    candidate,
                    pending_endpoint,
                    machines,
                    local_writer,
                )?;
            }
            return Ok(true);
        }
    }
    // The server owns the sidebar seam and dock dividers. Forward every mouse
    // event that did not activate a machine row so resize drags behave exactly
    // like they do beside ordinary workspace rows.
    Ok(false)
}

fn workspace_hit_target(
    hit: &DockHit,
    dock: &DockState,
    machines: &HashMap<String, MachineRuntime>,
) -> Option<(Endpoint, String)> {
    match hit {
        DockHit::LocalWorkspace(index) => dock
            .local_workspaces
            .iter()
            .find(|workspace| workspace.index == *index)
            .map(|workspace| (Endpoint::Local, workspace.id.clone())),
        DockHit::RemoteWorkspace(
            endpoint @ Endpoint::Remote {
                machine_id,
                session,
            },
            index,
        ) => machines
            .get(machine_id)?
            .endpoint(session)?
            .workspaces
            .iter()
            .find(|workspace| workspace.index == *index)
            .map(|workspace| (endpoint.clone(), workspace.id.clone())),
        _ => None,
    }
}

fn machine_can_disconnect(state: &MachineState) -> bool {
    matches!(
        state,
        MachineState::Online | MachineState::Connecting { .. } | MachineState::Reconnecting { .. }
    )
}

fn disconnect_machine_runtime(runtime: &mut SessionRuntime) {
    runtime.generation = next_connection_generation();
    runtime.state = MachineState::Disabled;
    stop_link(runtime);
}

fn move_machine_popup_selection(selected: &mut usize, code: &KeyCode) -> bool {
    if matches!(
        code,
        KeyCode::Up
            | KeyCode::Down
            | KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Char('j')
            | KeyCode::Char('k')
    ) {
        *selected = 1 - *selected;
        true
    } else {
        false
    }
}

fn advance_machine_popup(dock: &mut DockState, events: &Sender<ShellEvent>) -> Result<()> {
    match dock.machine_popup.take() {
        Some(MachinePopup::Menu {
            machine_id, label, ..
        }) => {
            dock.machine_popup = Some(MachinePopup::Confirm {
                machine_id,
                label,
                rect: None,
                confirm_rect: None,
                cancel_rect: None,
                removing: false,
                error: None,
            });
            dock.dirty = true;
        }
        Some(MachinePopup::Confirm {
            machine_id,
            label,
            rect,
            confirm_rect,
            cancel_rect,
            removing,
            error,
        }) => {
            if removing {
                dock.machine_popup = Some(MachinePopup::Confirm {
                    machine_id,
                    label,
                    rect,
                    confirm_rect,
                    cancel_rect,
                    removing,
                    error,
                });
                return Ok(());
            }
            let worker_id = machine_id.clone();
            dock.machine_popup = Some(MachinePopup::Confirm {
                machine_id,
                label,
                rect,
                confirm_rect,
                cancel_rect,
                removing: true,
                error: None,
            });
            dock.dirty = true;
            let events = events.clone();
            std::thread::Builder::new()
                .name("machine-remove".to_string())
                .stack_size(256 * 1024)
                .spawn(move || {
                    let result = crate::machine::remove_profile(&worker_id)
                        .map_err(|error| error.to_string());
                    let _ = events.send(ShellEvent::MachineRemoved {
                        machine_id: worker_id,
                        result,
                    });
                })?;
        }
        None => {}
    }
    Ok(())
}

fn machine_popup_removing(dock: &DockState) -> bool {
    matches!(
        dock.machine_popup,
        Some(MachinePopup::Confirm { removing: true, .. })
    )
}

fn machine_form_submitting(dock: &DockState) -> bool {
    dock.machine_form
        .as_ref()
        .is_some_and(|form| form.submitting)
}

fn set_machine_form_error(dock: &mut DockState, message: &str) {
    if let Some(form) = dock.machine_form.as_mut() {
        form.submitting = false;
        form.error = Some(message.to_string());
        dock.dirty = true;
    }
}

fn machine_form_awaiting_approval(dock: &DockState) -> bool {
    dock.machine_form
        .as_ref()
        .is_some_and(|form| form.install_prompt.is_some())
}

fn install_prompt_choice(code: &KeyCode) -> Option<bool> {
    match code {
        KeyCode::Char('y' | 'Y') => Some(true),
        KeyCode::Char('n' | 'N') | KeyCode::Enter | KeyCode::Esc => Some(false),
        _ => None,
    }
}

fn clear_install_prompt(dock: &mut DockState) {
    if let Some(form) = dock.machine_form.as_mut() {
        form.install_prompt = None;
        form.submitting = false;
        form.error = None;
    }
    dock.dirty = true;
}

fn move_machine_form_cursor(dock: &mut DockState, delta: i32) {
    if let Some(form) = dock.machine_form.as_mut() {
        let max = form
            .fields
            .len()
            .saturating_add(dock.saved_ids.len())
            .saturating_sub(1) as i32;
        form.cursor = (form.cursor as i32 + delta).clamp(0, max) as usize;
        form.selected_saved = form
            .cursor
            .checked_sub(form.fields.len())
            .and_then(|index| dock.saved_ids.get(index).cloned());
        form.error = None;
        dock.dirty = true;
    }
}

fn active_machine_form_field(form: &MachineForm) -> Option<(&str, usize)> {
    let field = form.fields.get(form.cursor)?;
    let cursor = form
        .edit_cursors
        .get(form.cursor)
        .copied()
        .unwrap_or_default()
        .min(field.chars().count());
    Some((field, cursor))
}

fn char_byte_index(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map_or(text.len(), |(index, _)| index)
}

fn previous_word_boundary(text: &str, cursor: usize) -> usize {
    let chars = text.chars().collect::<Vec<_>>();
    let mut index = cursor.min(chars.len());
    while index > 0 && chars[index - 1].is_whitespace() {
        index -= 1;
    }
    while index > 0 && !chars[index - 1].is_whitespace() {
        index -= 1;
    }
    index
}

fn next_word_boundary(text: &str, cursor: usize) -> usize {
    let chars = text.chars().collect::<Vec<_>>();
    let mut index = cursor.min(chars.len());
    while index < chars.len() && chars[index].is_whitespace() {
        index += 1;
    }
    while index < chars.len() && !chars[index].is_whitespace() {
        index += 1;
    }
    index
}

fn machine_form_line_modifier(modifiers: KeyModifiers) -> bool {
    modifiers.intersects(KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META)
}

fn machine_form_word_modifier(modifiers: KeyModifiers) -> bool {
    modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL)
}

fn move_machine_form_edit_cursor(dock: &mut DockState, delta: i32, modifiers: KeyModifiers) {
    let Some(form) = dock.machine_form.as_mut() else {
        return;
    };
    let Some((field, cursor)) = active_machine_form_field(form) else {
        return;
    };
    let next = if machine_form_line_modifier(modifiers) {
        if delta < 0 {
            0
        } else {
            field.chars().count()
        }
    } else if machine_form_word_modifier(modifiers) {
        if delta < 0 {
            previous_word_boundary(field, cursor)
        } else {
            next_word_boundary(field, cursor)
        }
    } else {
        (cursor as i32 + delta).clamp(0, field.chars().count() as i32) as usize
    };
    form.edit_cursors[form.cursor] = next;
    form.error = None;
    dock.dirty = true;
}

fn move_machine_form_edit_cursor_to_edge(dock: &mut DockState, end: bool) {
    let Some(form) = dock.machine_form.as_mut() else {
        return;
    };
    let Some((field, _)) = active_machine_form_field(form) else {
        return;
    };
    form.edit_cursors[form.cursor] = if end { field.chars().count() } else { 0 };
    form.error = None;
    dock.dirty = true;
}

fn delete_machine_form_text(dock: &mut DockState, forward: bool, modifiers: KeyModifiers) {
    let Some(form) = dock.machine_form.as_mut() else {
        return;
    };
    let field_index = form.cursor;
    let Some(field) = form.fields.get_mut(field_index) else {
        return;
    };
    let cursor = form.edit_cursors[field_index].min(field.chars().count());
    let char_count = field.chars().count();
    let (start, end) = if machine_form_line_modifier(modifiers) {
        if forward {
            (cursor, char_count)
        } else {
            (0, cursor)
        }
    } else if machine_form_word_modifier(modifiers) {
        if forward {
            (cursor, next_word_boundary(field, cursor))
        } else {
            (previous_word_boundary(field, cursor), cursor)
        }
    } else if forward {
        (cursor, cursor.saturating_add(1).min(char_count))
    } else {
        (cursor.saturating_sub(1), cursor)
    };
    if start < end {
        let byte_start = char_byte_index(field, start);
        let byte_end = char_byte_index(field, end);
        field.replace_range(byte_start..byte_end, "");
        form.edit_cursors[field_index] = start;
    }
    form.error = None;
    dock.dirty = true;
}

fn append_machine_form_text(dock: &mut DockState, text: &str) {
    const LIMITS: [usize; 3] = [64, 255, 64];
    let Some(form) = dock.machine_form.as_mut() else {
        return;
    };
    let field_index = form.cursor;
    let Some(field) = form.fields.get_mut(field_index) else {
        return;
    };
    let cursor = form.edit_cursors[field_index].min(field.chars().count());
    let available = LIMITS[field_index].saturating_sub(field.chars().count());
    let inserted = text
        .chars()
        .filter(|character| !character.is_control())
        .take(available)
        .collect::<String>();
    if !inserted.is_empty() {
        let inserted_chars = inserted.chars().count();
        let byte_index = char_byte_index(field, cursor);
        field.insert_str(byte_index, &inserted);
        form.edit_cursors[field_index] = cursor + inserted_chars;
    }
    form.error = None;
    dock.dirty = true;
}

fn switch_to_workspace_tab(
    dock: &mut DockState,
    active: &Endpoint,
    candidate: &mut Option<SurfaceCandidate>,
    machines: &HashMap<String, MachineRuntime>,
    writer: &Arc<Mutex<crate::ipc::transport::Conn>>,
) -> Result<()> {
    let draft = dock.machine_form.take();
    close_selector(dock, active, candidate, machines, writer)?;
    dock.machine_draft = draft;
    Ok(())
}

fn cancel_machine_form(
    dock: &mut DockState,
    active: &Endpoint,
    candidate: &mut Option<SurfaceCandidate>,
    machines: &HashMap<String, MachineRuntime>,
    writer: &Arc<Mutex<crate::ipc::transport::Conn>>,
) -> Result<()> {
    // The selected server still owns the native picker underneath this form.
    // Close that layer first so cancelling dismisses the complete creation
    // surface instead of revealing a stale picker on the next full frame.
    send_surface(
        active,
        &ClientMessage::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        machines,
        writer,
    )?;
    dock.machine_draft = None;
    close_selector(dock, active, candidate, machines, writer)
}

fn activate_machine_form(
    dock: &mut DockState,
    events: &Sender<ShellEvent>,
    pending: &mut Option<Endpoint>,
    machines: &mut HashMap<String, MachineRuntime>,
    approved: bool,
) -> Result<()> {
    let Some(id) = dock
        .machine_form
        .as_ref()
        .and_then(|form| form.selected_saved.clone())
    else {
        return submit_machine_form(dock, events, approved);
    };
    let Some(machine) = machines.get_mut(&id) else {
        set_machine_form_error(dock, "This saved machine was removed.");
        return Ok(());
    };
    if machine.profile.enabled && matches!(machine.runtime.state, MachineState::Online) {
        *pending = Some(Endpoint::Remote {
            machine_id: id,
            session: machine.session(),
        });
    } else {
        let events = events.clone();
        std::thread::Builder::new()
            .name("machine-enable".into())
            .stack_size(512 * 1024)
            .spawn(move || {
                let result = crate::machine::enable_profile(&id, approved)
                    .map_err(|error| error.to_string());
                let _ = events.send(ShellEvent::MachineCreated(result));
            })?;
    }
    if let Some(form) = dock.machine_form.as_mut() {
        form.submitting = true;
        form.error = None;
    }
    dock.dirty = true;
    Ok(())
}

fn submit_machine_form(
    dock: &mut DockState,
    events: &Sender<ShellEvent>,
    approved: bool,
) -> Result<()> {
    let Some(form) = dock.machine_form.as_mut() else {
        return Ok(());
    };
    let label = form.fields[0].trim().to_string();
    let destination = form.fields[1].trim().to_string();
    let session = (!form.fields[2].trim().is_empty()).then(|| form.fields[2].trim().to_string());
    if label.is_empty() || destination.is_empty() {
        form.error = Some("Machine name and SSH host are required.".to_string());
        dock.dirty = true;
        return Ok(());
    }
    form.submitting = true;
    form.install_prompt = None;
    form.error = None;
    dock.dirty = true;
    let events = events.clone();
    std::thread::Builder::new()
        .name("machine-create".to_string())
        .stack_size(512 * 1024)
        .spawn(move || {
            let result = crate::machine::add_profile(label, destination, session, approved)
                .map_err(|error| error.to_string());
            let _ = events.send(ShellEvent::MachineCreated(result));
        })?;
    Ok(())
}

fn selector_endpoints(machines: &HashMap<String, MachineRuntime>) -> Vec<Endpoint> {
    let mut rows = machines.values().collect::<Vec<_>>();
    rows.sort_unstable_by(|left, right| left.profile.label.cmp(&right.profile.label));
    let mut endpoints = vec![Endpoint::Local];
    for machine in rows {
        endpoints.push(Endpoint::Remote {
            machine_id: machine.profile.id.clone(),
            session: machine.session(),
        });
    }
    endpoints
}

fn machine_dock_rows(
    machines: &HashMap<String, MachineRuntime>,
    _active: &Endpoint,
    local_workspaces: &[protocol::ShellWorkspace],
) -> Vec<MachineDockRow> {
    let mut machines = machines.values().collect::<Vec<_>>();
    machines.sort_unstable_by(|left, right| {
        left.profile
            .label
            .cmp(&right.profile.label)
            .then_with(|| left.profile.id.cmp(&right.profile.id))
    });
    // This list is the client shell, not a projection of whichever endpoint is
    // active. Keep owner-local workspaces first and machine rows after them so
    // switching content never replaces or reorders the navigation surface.
    let mut rows = vec![MachineDockRow::LocalHeading];
    rows.extend(
        local_workspaces
            .iter()
            .cloned()
            .map(MachineDockRow::LocalWorkspace)
            .collect::<Vec<_>>(),
    );
    for machine in machines {
        rows.push(MachineDockRow::Machine(machine.profile.id.clone()));
        let endpoint = Endpoint::Remote {
            machine_id: machine.profile.id.clone(),
            session: machine.session(),
        };
        rows.extend(
            machine
                .runtime
                .workspaces
                .iter()
                .cloned()
                .map(|workspace| MachineDockRow::RemoteWorkspace(endpoint.clone(), workspace)),
        );
    }
    rows
}

fn visible_dock_rows(
    machines: &HashMap<String, MachineRuntime>,
    active: &Endpoint,
    dock: &DockState,
) -> Vec<MachineDockRow> {
    machine_dock_rows(machines, active, &dock.local_workspaces)
        .into_iter()
        .filter(|row| match row {
            MachineDockRow::LocalWorkspace(_) => !dock.local_collapsed,
            MachineDockRow::RemoteWorkspace(Endpoint::Remote { machine_id, .. }, _) => {
                !dock.collapsed_machines.contains(machine_id)
            }
            _ => true,
        })
        .collect()
}

fn dock_row_height(row: &MachineDockRow, paths: bool) -> u16 {
    if paths
        && matches!(
            row,
            MachineDockRow::Machine(_)
                | MachineDockRow::LocalWorkspace(_)
                | MachineDockRow::RemoteWorkspace(_, _)
        )
    {
        2
    } else {
        1
    }
}

fn dock_row_hit(row: &MachineDockRow) -> DockHit {
    match row {
        MachineDockRow::LocalHeading => DockHit::Endpoint(Endpoint::Local),
        MachineDockRow::Machine(id) => DockHit::Machine(id.clone()),
        MachineDockRow::LocalWorkspace(ws) => DockHit::LocalWorkspace(ws.index),
        MachineDockRow::RemoteWorkspace(endpoint, ws) => {
            DockHit::RemoteWorkspace(endpoint.clone(), ws.index)
        }
    }
}

fn claim_pointer_navigation(dock: &mut DockState, hover: Option<&DockHit>) -> bool {
    let next_hover = hover.cloned();
    let changed = dock.hover != next_hover;
    let release_server_focus = dock.navigation.take().is_some();
    if changed {
        dock.hover = next_hover;
    }
    if changed || release_server_focus {
        dock.dirty = true;
    }
    release_server_focus
}

fn update_client_sidebar_drag(dock: &mut DockState, column: u16) -> bool {
    let Some(drag) = dock.width_drag else {
        return false;
    };
    let width = resized_workspace_width(drag, column);
    let Some(state) = dock.sidebars.as_mut() else {
        return false;
    };
    let side = if drag.left {
        &mut state.layout.left
    } else {
        &mut state.layout.right
    };
    if side.width == width {
        return false;
    }
    side.width = width;
    state.revision = state.revision.saturating_add(1);
    true
}

fn send_latest_sidebar_state(
    dock: &mut DockState,
    active: &Endpoint,
    machines: &HashMap<String, MachineRuntime>,
    local_writer: &Arc<Mutex<crate::ipc::transport::Conn>>,
) {
    if !matches!(active, Endpoint::Remote { .. }) || dock.sidebar_sync.is_some() {
        return;
    }
    let Some(state) = dock.sidebars.clone() else {
        return;
    };
    if send_surface(
        active,
        &ClientMessage::ShellSidebars(state.clone()),
        machines,
        local_writer,
    )
    .is_ok()
    {
        dock.sidebar_sync = Some((active.clone(), state.revision));
    }
}

fn has_newer_sidebar_revision(dock: &DockState, acknowledged: u64) -> bool {
    dock.sidebars
        .as_ref()
        .is_some_and(|current| current.revision > acknowledged)
}

fn reveal_workspace(
    rows: &[MachineDockRow],
    active: &Endpoint,
    dock: &mut DockState,
    height: u16,
    paths: bool,
) {
    if let Some(index) = dock.navigation {
        if !dock.navigation_reveal {
            return;
        }
        dock.navigation_reveal = false;
        let index = index.min(rows.len().saturating_sub(1));
        dock.navigation = Some(index);
        dock.scroll = dock.scroll.min(index);
        while dock.scroll < index
            && rows[dock.scroll..=index]
                .iter()
                .map(|row| usize::from(dock_row_height(row, paths)))
                .sum::<usize>()
                > usize::from(height)
        {
            dock.scroll += 1;
        }
        return;
    }
    let target = |selected: bool| {
        rows.iter().enumerate().find_map(|(index, row)| {
            let (endpoint, ws) = match row {
                MachineDockRow::LocalWorkspace(ws) => (Endpoint::Local, ws),
                MachineDockRow::RemoteWorkspace(endpoint, ws) => (endpoint.clone(), ws),
                _ => return None,
            };
            (same_selection(&endpoint, active) && if selected { ws.selected } else { ws.active })
                .then(|| (index, (endpoint, ws.id.clone(), selected)))
        })
    };
    let Some((index, identity)) = target(true).or_else(|| target(false)) else {
        return;
    };
    if dock.revealed_workspace.as_ref() == Some(&identity) {
        return;
    }
    dock.revealed_workspace = Some(identity);
    if index < dock.scroll {
        dock.scroll = index;
    }
    while dock.scroll < index
        && rows[dock.scroll..=index]
            .iter()
            .map(|row| usize::from(dock_row_height(row, paths)))
            .sum::<usize>()
            > usize::from(height)
    {
        dock.scroll += 1;
    }
}

fn dock_max_scroll(rows: &[MachineDockRow], height: u16, paths: bool) -> usize {
    let mut used = 0;
    let mut start = rows.len();
    for row in rows.iter().rev() {
        let next = dock_row_height(row, paths);
        if used + next > height {
            break;
        }
        used += next;
        start -= 1;
    }
    start.min(rows.len().saturating_sub(1))
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
    dock.machine_form = None;
    dock.machine_form_rect = None;
    dock.machine_form_hits.clear();
    dock.machine_form_backdrop_pending = false;
    dock.machine_popup = None;
    dock.refresh_catalog = true;
    dock.dirty = true;
}

#[allow(clippy::too_many_arguments)]
fn refresh_catalog(
    machines: &mut HashMap<String, MachineRuntime>,
    active: &mut Endpoint,
    candidate: &mut Option<SurfaceCandidate>,
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
    dock.catalog_revision = dock.catalog_revision.max(loaded.catalog.revision);
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
                .is_none_or(|profile| !runtime.profile.same_connection(profile))
        })
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();

    for id in &replaced {
        if candidate.as_ref().is_some_and(|candidate| {
            matches!(&candidate.endpoint, Endpoint::Remote { machine_id, .. } if machine_id == id)
        }) {
            candidate.take();
        }
        if let Some(mut runtime) = machines.remove(id) {
            stop_link(&mut runtime.runtime);
        }
    }

    let active_removed =
        matches!(active, Endpoint::Remote { machine_id, .. } if replaced.contains(machine_id));
    for (id, profile) in next {
        machines
            .entry(id)
            .and_modify(|runtime| runtime.profile.label.clone_from(&profile.label))
            .or_insert_with(|| new_machine_runtime(profile));
    }
    if active_removed {
        request_switch(
            Endpoint::Local,
            candidate,
            machines,
            dock.local_workspaces.len(),
            dock.sidebars.as_ref(),
            local_writer,
        )?;
    }
    let rows = projected_dock_row_count(machines, &Endpoint::Local, 0);
    if rows != *dock_rows {
        *dock_rows = rows;
        send_local(
            local_writer,
            &ClientMessage::ShellDockLayout(dock_layout(&Endpoint::Local, machines, 0)),
        )?;
        if !matches!(active, Endpoint::Local) {
            let _ = send_surface(
                active,
                &ClientMessage::ShellDockLayout(dock_layout(
                    active,
                    machines,
                    dock.local_workspaces.len(),
                )),
                machines,
                local_writer,
            );
        }
    }
    dock.dirty = true;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn try_open_pending_endpoint(
    pending: &mut Option<Endpoint>,
    dock: &mut DockState,
    candidate: &mut Option<SurfaceCandidate>,
    machines: &mut HashMap<String, MachineRuntime>,
    local_writer: &Arc<Mutex<crate::ipc::transport::Conn>>,
) -> Result<()> {
    let Some(endpoint) = pending.as_ref() else {
        return Ok(());
    };
    let Endpoint::Remote {
        machine_id,
        session,
    } = endpoint
    else {
        *pending = None;
        return Ok(());
    };
    let Some(runtime) = machines
        .get(machine_id)
        .and_then(|machine| machine.endpoint(session))
    else {
        return Ok(());
    };
    match &runtime.state {
        MachineState::Online => {
            request_switch(
                endpoint.clone(),
                candidate,
                machines,
                dock.local_workspaces.len(),
                dock.sidebars.as_ref(),
                local_writer,
            )?;
            *pending = None;
            dock.dirty = true;
        }
        MachineState::Attention(error) => {
            if let Some(form) = dock.machine_form.as_mut() {
                form.submitting = false;
                form.error = Some(error.clone());
            }
            dock.warning = Some(error.clone());
            dock.dirty = true;
        }
        MachineState::Disabled => {
            if let Some(form) = dock.machine_form.as_mut() {
                form.submitting = false;
                form.error = Some("The remote machine is disabled.".to_string());
            }
            *pending = None;
            dock.dirty = true;
        }
        MachineState::Connecting { .. } | MachineState::Reconnecting { .. } => {}
    }
    Ok(())
}

fn close_selector(
    dock: &mut DockState,
    active: &Endpoint,
    candidate: &mut Option<SurfaceCandidate>,
    machines: &HashMap<String, MachineRuntime>,
    local_writer: &Arc<Mutex<crate::ipc::transport::Conn>>,
) -> Result<()> {
    dock.selector_open = false;
    dock.selector_rect = None;
    dock.machine_form = None;
    dock.machine_form_rect = None;
    dock.machine_form_hits.clear();
    dock.machine_form_backdrop_pending = false;
    dock.machine_popup = None;
    dock.dirty = true;
    // Frames received beneath the client-owned selector were intentionally not
    // painted. Re-entering Active through Prepared invalidates the server-side
    // baseline and guarantees one complete frame before later diffs.
    let restored = (|| {
        send_interest(active, SurfaceInterest::Prepared, machines, local_writer)?;
        if let Ok((cols, rows)) = crossterm::terminal::size() {
            send_surface(
                active,
                &super::client::resize_message(cols, rows),
                machines,
                local_writer,
            )?;
        }
        send_interest(active, SurfaceInterest::Active, machines, local_writer)
    })();
    if let Err(error) = restored {
        if matches!(active, Endpoint::Local) {
            return Err(error);
        }
        dock.warning = Some("active machine connection lost".to_string());
        request_switch(
            Endpoint::Local,
            candidate,
            machines,
            dock.local_workspaces.len(),
            dock.sidebars.as_ref(),
            local_writer,
        )?;
    }
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

fn owner_local_session_switch(
    endpoint: &Endpoint,
    name: String,
) -> Option<super::client::ClientExit> {
    // Named sessions remain the owner-local namespace. A machine contributes
    // remote workspaces to it; a remote server cannot replace the machine's
    // internal backing endpoint by emitting its own SwitchSession message.
    matches!(endpoint, Endpoint::Local).then_some(super::client::ClientExit::SwitchSession(name))
}

#[allow(clippy::too_many_arguments)]
fn request_user_switch(
    endpoint: Endpoint,
    dock: &mut DockState,
    _active: &Endpoint,
    candidate: &mut Option<SurfaceCandidate>,
    pending_endpoint: &mut Option<Endpoint>,
    machines: &mut HashMap<String, MachineRuntime>,
    local_writer: &Arc<Mutex<crate::ipc::transport::Conn>>,
) -> Result<bool> {
    if let Endpoint::Remote {
        machine_id,
        session,
    } = &endpoint
    {
        let machine = machines
            .get_mut(machine_id)
            .ok_or_else(|| anyhow!("unknown machine `{machine_id}`"))?;
        let label = machine.profile.label.clone();
        let runtime = machine
            .endpoint_mut(session)
            .ok_or_else(|| anyhow!("unknown remote session `{session}`"))?;
        if !matches!(runtime.state, MachineState::Online) {
            let detail = runtime
                .attention_reason()
                .map(str::to_string)
                .unwrap_or_else(|| runtime.status().to_string());
            match runtime.state {
                MachineState::Disabled => {
                    dock.warning = Some(format!("{label} is disabled"));
                    return Ok(false);
                }
                MachineState::Connecting { .. } => {}
                MachineState::Reconnecting { .. } => {
                    runtime.state = MachineState::Reconnecting { at: Instant::now() };
                }
                MachineState::Attention(_) => {
                    stop_link(runtime);
                    runtime.state = MachineState::Reconnecting { at: Instant::now() };
                    runtime.backoff = Duration::from_secs(1);
                    runtime.handshake_failures = 0;
                }
                MachineState::Online => unreachable!(),
            }
            *pending_endpoint = Some(endpoint.clone());
            dock.warning = Some(
                if matches!(runtime.state, MachineState::Connecting { .. }) {
                    format!("Connecting to {label} ({detail})")
                } else {
                    format!("Reconnecting to {label} ({detail})")
                },
            );
            dock.dirty = true;
            return Ok(false);
        }
    }
    dock.warning = None;
    request_switch(
        endpoint,
        candidate,
        machines,
        dock.local_workspaces.len(),
        dock.sidebars.as_ref(),
        local_writer,
    )?;
    Ok(true)
}

fn request_switch(
    endpoint: Endpoint,
    candidate: &mut Option<SurfaceCandidate>,
    machines: &HashMap<String, MachineRuntime>,
    local_workspace_count: usize,
    sidebars: Option<&protocol::ShellSidebars>,
    local_writer: &Arc<Mutex<crate::ipc::transport::Conn>>,
) -> Result<()> {
    if let Some(previous) = candidate.take() {
        let _ = send_interest(
            &previous.endpoint,
            SurfaceInterest::Suspended,
            machines,
            local_writer,
        );
    }
    match &endpoint {
        Endpoint::Local => {
            if let Some(state) = sidebars {
                send_local(local_writer, &ClientMessage::ShellSidebars(state.clone()))?;
            }
            send_local(
                local_writer,
                &ClientMessage::ShellDockLayout(dock_layout(&endpoint, machines, 0)),
            )?;
            send_local(
                local_writer,
                &ClientMessage::SurfaceInterest(SurfaceInterest::Prepared),
            )?;
            if let Ok((cols, rows)) = crossterm::terminal::size() {
                send_local(local_writer, &super::client::resize_message(cols, rows))?;
            }
            *candidate = Some(SurfaceCandidate {
                ticket: next_connection_generation(),
                endpoint,
                welcomed: true,
                ready: true,
                shell_dock: None,
                boot_id: None,
                deadline: Instant::now() + SURFACE_PREPARE_TIMEOUT,
            });
        }
        Endpoint::Remote {
            machine_id,
            session,
        } => {
            let runtime = machines
                .get(machine_id)
                .and_then(|machine| machine.endpoint(session))
                .ok_or_else(|| anyhow!("unknown remote session `{session}`"))?;
            if !matches!(runtime.state, MachineState::Online) {
                return Ok(());
            }
            let control = runtime
                .control
                .as_ref()
                .ok_or_else(|| anyhow!("remote session connection is not ready"))?;
            if let Some(state) = sidebars {
                control.send(&ClientMessage::ShellSidebars(state.clone()))?;
            }
            control.send(&ClientMessage::ShellDockLayout(dock_layout(
                &endpoint,
                machines,
                local_workspace_count,
            )))?;
            control.send(&ClientMessage::SurfaceInterest(SurfaceInterest::Prepared))?;
            if let Ok((cols, rows)) = crossterm::terminal::size() {
                control.send(&super::client::resize_message(cols, rows))?;
            }
            *candidate = Some(SurfaceCandidate {
                ticket: next_connection_generation(),
                endpoint,
                welcomed: true,
                ready: true,
                shell_dock: None,
                boot_id: runtime.boot_id,
                deadline: Instant::now() + SURFACE_PREPARE_TIMEOUT,
            });
        }
    }
    if let Some(candidate) = candidate.as_ref() {
        let (cols, rows) = crossterm::terminal::size()?;
        send_surface(
            &candidate.endpoint,
            &ClientMessage::PrepareSurface {
                ticket: candidate.ticket,
                cols,
                rows,
            },
            machines,
            local_writer,
        )?;
    }
    Ok(())
}

fn commit_candidate(
    endpoint: Endpoint,
    active: &mut Endpoint,
    candidate: &mut Option<SurfaceCandidate>,
    machines: &HashMap<String, MachineRuntime>,
    local_writer: &Arc<Mutex<crate::ipc::transport::Conn>>,
) -> Result<()> {
    send_interest(&endpoint, SurfaceInterest::Active, machines, local_writer)?;
    // Once a complete candidate frame is valid, suspend the previous direct
    // endpoint. Its SSH connection remains alive without rendering or input.
    if *active != endpoint {
        let _ = send_interest(active, SurfaceInterest::Suspended, machines, local_writer);
    }
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
            session,
        } => machines
            .get(machine_id)
            .and_then(|machine| machine.endpoint(session))
            .and_then(|runtime| runtime.control.as_ref())
            .ok_or_else(|| anyhow!("remote session connection is unavailable"))?
            .send(message),
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

fn stop_link(endpoint: &mut SessionRuntime) {
    if let Some(control) = endpoint.control.take() {
        control.close();
    }
    if let Some(reader) = endpoint.reader.take() {
        // The link reader owns child reaping. Closing it unblocks IO without
        // waiting on a remote process in the client event loop.
        drop(reader);
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
    paint_dock_content(terminal, dock, machines, active, cursor, cursor_visible)?;
    paint_owner_session_button(terminal, dock, active, cursor, cursor_visible)
}

fn paint_dock_content(
    terminal: &mut DefaultTerminal,
    dock: &mut DockState,
    machines: &HashMap<String, MachineRuntime>,
    active: &Endpoint,
    cursor: &mut Option<(u16, u16)>,
    cursor_visible: bool,
) -> Result<()> {
    let machine_form_open = dock.machine_form.is_some();
    if !machine_form_open && dock.selector_open {
        return paint_selector(terminal, dock, machines, active, cursor);
    }
    if !machine_form_open && dock.machine_popup.is_some() {
        return paint_machine_popup(terminal, dock, machines, cursor);
    }
    if machines.is_empty() {
        // No machine navigation means the server's existing Workspaces UI
        // remains pixel-for-pixel authoritative, including its hit targets.
        dock.hits.clear();
        dock.dirty = false;
        return if machine_form_open {
            paint_machine_form(terminal, dock, machines, cursor)
        } else {
            Ok(())
        };
    }
    let Some(rect) = dock.rect else {
        dock.dirty = false;
        return if machine_form_open {
            paint_machine_form(terminal, dock, machines, cursor)
        } else {
            Ok(())
        };
    };
    let style = dock.owner_style.unwrap_or(rect);
    let paint_base = dock.owner_base.as_ref().unwrap_or(&dock.base).clone();
    let content_width = rect.width.saturating_sub(1);
    let geometry = Rect::new(rect.x, rect.y, content_width, rect.height);
    let mut cells = Vec::with_capacity(usize::from(content_width) * usize::from(rect.height));
    for y in rect.y..rect.y.saturating_add(rect.height) {
        for x in rect.x..rect.x.saturating_add(content_width) {
            let mut cell = paint_base.clone();
            cell.set_symbol(" ");
            cells.push((x, y, cell));
        }
    }
    dock.hits.clear();
    write_endpoint_text(
        &mut cells,
        geometry,
        0,
        2,
        dock.heading,
        StyleSpec {
            fg: protocol::unpack(style.secondary_fg),
            bg: None,
            bold: true,
        },
    );
    if rect.width >= 8 {
        let add = Rect::new(rect.x + rect.width.saturating_sub(4), rect.y, 3, 1);
        write_endpoint_text(
            &mut cells,
            geometry,
            0,
            rect.width.saturating_sub(4),
            " + ",
            StyleSpec {
                fg: protocol::unpack(style.active_fg),
                bg: Some(protocol::unpack(style.active_bg)),
                bold: true,
            },
        );
        dock.hits.push((DockHit::AddWorkspace, add));
    }
    let list_height = rect.height.saturating_sub(1);
    let rows = visible_dock_rows(machines, active, dock);
    reveal_workspace(&rows, active, dock, list_height, rect.show_paths);
    dock.scroll = dock
        .scroll
        .min(dock_max_scroll(&rows, list_height, rect.show_paths));
    let mut next_row = 1;
    let total_rows = rows.len();
    let mut visible_rows = 0;
    for (row_index, projected) in rows.into_iter().enumerate().skip(dock.scroll) {
        let stride = dock_row_height(&projected, rect.show_paths);
        let row = next_row;
        if row + stride > rect.height {
            break;
        }
        next_row += stride;
        visible_rows += 1;
        let workspace = match &projected {
            MachineDockRow::LocalWorkspace(ws) => {
                Some((Endpoint::Local, ws, DockHit::LocalWorkspace(ws.index), true))
            }
            MachineDockRow::RemoteWorkspace(endpoint, ws) => {
                let online = match endpoint {
                    Endpoint::Remote {
                        machine_id,
                        session,
                    } => machines
                        .get(machine_id)
                        .and_then(|machine| machine.endpoint(session))
                        .is_some_and(|runtime| matches!(runtime.state, MachineState::Online)),
                    Endpoint::Local => false,
                };
                Some((
                    endpoint.clone(),
                    ws,
                    DockHit::RemoteWorkspace(endpoint.clone(), ws.index),
                    online,
                ))
            }
            _ => None,
        };
        if let Some((endpoint, ws, hit, online)) = workspace {
            let area = Rect::new(rect.x, rect.y + row, rect.width, stride);
            dock.workspace_buffer.resize(area);
            for cell in &mut dock.workspace_buffer.content {
                *cell = paint_base.clone();
                cell.set_symbol(" ");
            }
            let selected_endpoint = same_selection(&endpoint, active) && online;
            crate::ui::workspace_row::draw(
                &mut dock.workspace_buffer,
                area,
                crate::ui::workspace_row::WorkspaceRow {
                    name: &ws.name,
                    branch: ws.branch.as_deref(),
                    path: &ws.cwd,
                    dot: &ws.dot,
                    dot_color: protocol::unpack(if online {
                        ws.dot_color
                    } else {
                        style.secondary_fg
                    }),
                    nested: ws.nested,
                    active: selected_endpoint && ws.active,
                    selected: dock
                        .navigation
                        .map_or(selected_endpoint && ws.selected, |index| index == row_index),
                    hovered: dock.hover.as_ref() == Some(&hit),
                },
                crate::ui::workspace_row::Palette {
                    accent: protocol::unpack(style.active_fg),
                    normal: protocol::unpack(if online {
                        style.normal_fg
                    } else {
                        style.secondary_fg
                    }),
                    muted: protocol::unpack(style.secondary_fg),
                    path: protocol::unpack(style.active_secondary_fg),
                    branch: protocol::unpack(style.branch_fg),
                    active_bg: protocol::unpack(style.active_bg),
                    selected_bg: protocol::unpack(style.chrome.rule),
                },
            );
            for y in area.y..area.bottom() {
                for x in area.x..area.right().saturating_sub(1) {
                    let index = usize::from(y - rect.y) * usize::from(content_width)
                        + usize::from(x - rect.x);
                    cells[index].2.clone_from(&dock.workspace_buffer[(x, y)]);
                }
            }
            dock.hits
                .push((hit, Rect::new(area.x, area.y, content_width, stride)));
            continue;
        }
        let (hit, label, secondary_text, symbol, tone, selected, bold) = match projected {
            MachineDockRow::LocalHeading => (
                DockHit::Endpoint(Endpoint::Local),
                "Local".to_string(),
                None,
                if dock.local_collapsed { "▸" } else { "▾" },
                protocol::unpack(style.normal_fg),
                false,
                true,
            ),
            MachineDockRow::RemoteWorkspace(_, _) | MachineDockRow::LocalWorkspace(_) => {
                unreachable!("workspace rows use the shared native renderer")
            }
            MachineDockRow::Machine(machine_id) => {
                let machine = &machines[&machine_id];
                let machine_active = matches!(active, Endpoint::Remote { machine_id: active_id, .. } if active_id == &machine_id);
                let selected = machine_header_active(
                    machine_active,
                    dock.collapsed_machines.contains(&machine_id),
                );
                (
                    DockHit::Machine(machine_id.clone()),
                    machine.profile.label.clone(),
                    Some(machine.profile.destination.clone()),
                    if dock.collapsed_machines.contains(&machine_id) {
                        "▸"
                    } else {
                        "▾"
                    },
                    protocol::unpack(if selected {
                        style.active_fg
                    } else {
                        style.normal_fg
                    }),
                    selected,
                    selected,
                )
            }
        };
        let hovered = dock.hover.as_ref() == Some(&hit) || dock.navigation == Some(row_index);
        if selected || hovered {
            let bg = protocol::unpack(if selected {
                style.active_bg
            } else {
                style.chrome.surface
            });
            for (_, y, cell) in cells
                .iter_mut()
                .filter(|(_, y, _)| *y >= rect.y + row && *y < rect.y + row + stride)
            {
                let _ = y;
                cell.set_bg(bg);
            }
        }
        write_endpoint_text(
            &mut cells,
            geometry,
            row,
            2,
            symbol,
            StyleSpec {
                fg: tone,
                bg: selected.then(|| protocol::unpack(style.active_bg)),
                bold: false,
            },
        );
        let primary = StyleSpec {
            fg: protocol::unpack(if selected {
                style.active_fg
            } else {
                style.normal_fg
            }),
            bg: selected.then(|| protocol::unpack(style.active_bg)),
            bold,
        };
        write_endpoint_text(&mut cells, geometry, row, 4, &label, primary);
        if rect.show_paths {
            let secondary_style = StyleSpec {
                fg: protocol::unpack(if selected {
                    style.active_secondary_fg
                } else {
                    style.secondary_fg
                }),
                bg: selected.then(|| protocol::unpack(style.active_bg)),
                bold: false,
            };
            if let Some(value) = secondary_text.as_deref() {
                write_endpoint_text(&mut cells, geometry, row + 1, 4, value, secondary_style);
            }
        }
        dock.hits
            .push((hit, Rect::new(rect.x, rect.y + row, content_width, stride)));
    }

    if rect.width >= 3 && list_height > 0 {
        let track = Rect::new(rect.x + rect.width - 2, rect.y + 1, 1, list_height);
        dock.workspace_buffer.resize(track);
        for cell in &mut dock.workspace_buffer.content {
            *cell = paint_base.clone();
            cell.set_symbol(" ");
        }
        crate::ui::workspace_row::scrollbar(
            &mut dock.workspace_buffer,
            track,
            total_rows,
            visible_rows,
            dock.scroll,
            protocol::unpack(style.normal_fg),
            protocol::unpack(style.chrome.rule),
        );
        for y in track.y..track.bottom() {
            let index = usize::from(y - rect.y) * usize::from(content_width)
                + usize::from(track.x - rect.x);
            cells[index]
                .2
                .clone_from(&dock.workspace_buffer[(track.x, y)]);
        }
    }
    // Header pixels and hit targets belong exclusively to the active server.
    // Server-owned popups and modals remain on top of the client projection.
    // When those cells are restored to the Workspaces background by a later
    // frame diff, the occlusion map clears them and this painter fills the
    // endpoint text back in without a full-frame request.
    if let Some(overlay) = rect.overlay {
        if rect.overlay_dims_background {
            let dim_fg = protocol::unpack(style.chrome.divider);
            let dim_bg = protocol::unpack(style.chrome.accent_text);
            for (_, _, cell) in &mut cells {
                cell.set_fg(dim_fg);
                cell.set_bg(dim_bg);
            }
        }
        cells.retain(|(x, y, _)| !shell_block_contains(overlay, *x, *y));
    }
    super::client::sync_begin();
    super::client::paint(terminal, &cells, *cursor, cursor_visible, false, cursor)?;
    super::client::sync_end();
    if machine_form_open {
        paint_machine_form(terminal, dock, machines, cursor)
    } else {
        dock.dirty = false;
        Ok(())
    }
}

fn paint_owner_session_button(
    terminal: &mut DefaultTerminal,
    dock: &DockState,
    active: &Endpoint,
    cursor: &mut Option<(u16, u16)>,
    cursor_visible: bool,
) -> Result<()> {
    if matches!(active, Endpoint::Local) {
        return Ok(());
    }
    let Some((slot, button, label)) = owner_session_projection(dock) else {
        return Ok(());
    };
    let size = terminal.size()?;
    if slot.width == 0
        || slot.height == 0
        || slot.right() > size.width
        || slot.bottom() > size.height
        || !slot.contains((button.x, button.y).into())
        || button.right() > slot.right()
        || button.bottom() > slot.bottom()
    {
        return Ok(());
    }
    let Some(style) = dock.owner_style else {
        return Ok(());
    };
    let owner_background = dock.owner_base.as_ref().unwrap_or(&dock.base).bg;
    let mut cells = Vec::with_capacity(usize::from(slot.width) * usize::from(slot.height));
    for y in slot.y..slot.bottom() {
        for x in slot.x..slot.right() {
            let mut cell = dock.owner_base.as_ref().unwrap_or(&dock.base).clone();
            cell.set_symbol(" ");
            cell.set_fg(protocol::unpack(style.chrome.text));
            cell.modifier = Modifier::empty();
            cells.push((x, y, cell));
        }
    }
    write_endpoint_text(
        &mut cells,
        button,
        0,
        0,
        &label,
        StyleSpec {
            fg: protocol::unpack(if dock.owner_session_hovered {
                style.chrome.accent_text
            } else {
                style.chrome.text
            }),
            bg: Some(if dock.owner_session_hovered {
                protocol::unpack(style.chrome.accent)
            } else {
                owner_background
            }),
            bold: true,
        },
    );
    super::client::sync_begin();
    super::client::paint(terminal, &cells, *cursor, cursor_visible, false, cursor)?;
    super::client::sync_end();
    Ok(())
}

fn owner_session_projection(dock: &DockState) -> Option<(Rect, Rect, String)> {
    // Presence of the owner-local button is the authority to expose session
    // switching. Its geometry is not authoritative while a remote endpoint is
    // active because that endpoint may have a different client-owned sidebar
    // width. Clip and truncate against the active frame's reserved slot so the
    // repaint can never erase the adjacent Menu control.
    dock.owner_session_button?;
    let slot = dock
        .rect
        .and_then(|rect| rect.session_slot)
        .map(|slot| Rect::new(slot.x, slot.y, slot.width, slot.height))
        .or(dock.owner_session_slot)?;
    if slot.width == 0 || slot.height == 0 {
        return None;
    }
    let name = crate::ui::truncate(&dock.local_session, slot.width.saturating_sub(2) as usize);
    let label = format!(" {name} ");
    let width = (crate::ui::display_width(&label) as u16).min(slot.width);
    let button = Rect::new(slot.x, slot.y, width, 1.min(slot.height));
    Some((slot, button, label))
}

fn machine_header_active(endpoint_active: bool, collapsed: bool) -> bool {
    endpoint_active && collapsed
}

fn broadcast_workspace_width(
    dock: &DockState,
    machines: &HashMap<String, MachineRuntime>,
    local_writer: &Arc<Mutex<crate::ipc::transport::Conn>>,
) -> Result<()> {
    let local_message = ClientMessage::ShellDockLayout(ShellDockLayout {
        workspace_width: dock.workspace_width,
        owns_workspaces: !machines.is_empty(),
        ..ShellDockLayout::default()
    });
    send_local(local_writer, &local_message)?;
    let remote_message = ClientMessage::ShellDockLayout(ShellDockLayout {
        workspace_width: dock.workspace_width,
        owns_workspaces: !machines.is_empty(),
        owns_session_chrome: true,
        ..ShellDockLayout::default()
    });
    for machine in machines.values() {
        if !matches!(machine.runtime.state, MachineState::Online) {
            continue;
        }
        if let Some(control) = machine.runtime.control.as_ref() {
            // A disconnected sibling must not interrupt a drag on the active
            // surface. Its next Ready applies the retained client width.
            let _ = control.send(&remote_message);
        }
    }
    Ok(())
}

fn resized_workspace_width(drag: protocol::ShellResize, column: u16) -> u16 {
    let width = if drag.left {
        column.saturating_sub(drag.origin).saturating_add(1)
    } else {
        drag.origin.saturating_sub(column)
    };
    width.clamp(
        crate::app::SIDEBAR_WIDTH_MIN,
        drag.maximum.max(crate::app::SIDEBAR_WIDTH_MIN),
    )
}

fn paint_machine_popup(
    terminal: &mut DefaultTerminal,
    dock: &mut DockState,
    machines: &HashMap<String, MachineRuntime>,
    cursor: &mut Option<(u16, u16)>,
) -> Result<()> {
    let size = terminal.size()?;
    let theme = dock.form_theme;
    let surface = protocol::unpack(theme.surface);
    let border = protocol::unpack(theme.border);
    let text = protocol::unpack(theme.text);
    let subtext = protocol::unpack(theme.subtext1);
    let accent = protocol::unpack(theme.accent);
    let error_color = protocol::unpack(theme.error);

    let (rect, menu) = match dock.machine_popup.as_ref() {
        Some(MachinePopup::Menu { anchor, .. }) => {
            let width = display_columns(dock.delete_label).saturating_add(4).max(14);
            let x = anchor.0.min(size.width.saturating_sub(width));
            let y = anchor.1.min(size.height.saturating_sub(4));
            (
                Rect::new(x, y, width.min(size.width), 4.min(size.height)),
                true,
            )
        }
        Some(MachinePopup::Confirm { label, .. }) => {
            let prompt = format!("{} {}?", dock.delete_label, label);
            let width = display_columns(&prompt)
                .saturating_add(4)
                .clamp(30, 60)
                .min(size.width);
            let height = 6.min(size.height);
            (
                Rect::new(
                    size.width.saturating_sub(width) / 2,
                    size.height.saturating_sub(height) / 2,
                    width,
                    height,
                ),
                false,
            )
        }
        None => return Ok(()),
    };
    let mut cells = Vec::with_capacity(usize::from(rect.width) * usize::from(rect.height));
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            let mut cell = dock.base.clone();
            cell.set_symbol(" ");
            cell.set_fg(text);
            cell.set_bg(surface);
            cell.modifier = Modifier::empty();
            cells.push((x, y, cell));
        }
    }
    draw_popup_border(&mut cells, rect, border);

    if menu {
        let (connection_label, selected) = match dock.machine_popup.as_ref() {
            Some(MachinePopup::Menu {
                machine_id,
                selected,
                ..
            }) => (
                if machines
                    .get(machine_id)
                    .is_some_and(|machine| machine_can_disconnect(&machine.runtime.state))
                {
                    "Disconnect"
                } else {
                    "Connect"
                },
                *selected,
            ),
            _ => unreachable!(),
        };
        write_endpoint_text(
            &mut cells,
            rect,
            1,
            2,
            connection_label,
            StyleSpec {
                fg: if selected == 0 { accent } else { text },
                bg: None,
                bold: selected == 0,
            },
        );
        write_endpoint_text(
            &mut cells,
            rect,
            2,
            2,
            dock.delete_label,
            StyleSpec {
                fg: error_color,
                bg: None,
                bold: selected == 1,
            },
        );
        if let Some(MachinePopup::Menu {
            rect: popup_rect,
            remove_rect,
            ..
        }) = dock.machine_popup.as_mut()
        {
            *popup_rect = Some(rect);
            *remove_rect = Some(Rect::new(
                rect.x + 1,
                rect.y + 2,
                rect.width.saturating_sub(2),
                1,
            ));
        }
    } else {
        let (label, removing, message) = match dock.machine_popup.as_ref() {
            Some(MachinePopup::Confirm {
                label,
                removing,
                error,
                ..
            }) => (label.clone(), *removing, error.clone()),
            _ => unreachable!(),
        };
        let prompt = format!("{} {}?", dock.delete_label, label);
        write_endpoint_text(
            &mut cells,
            rect,
            1,
            2,
            &prompt,
            StyleSpec {
                fg: text,
                bg: None,
                bold: true,
            },
        );
        for column in 1..rect.width.saturating_sub(1) {
            set_form_symbol(
                &mut cells,
                rect,
                column,
                3,
                "─",
                protocol::unpack(theme.rule),
                false,
            );
        }
        let has_error = message.is_some();
        let footer = if removing {
            dock.working_label.to_string()
        } else if let Some(message) = message {
            truncate_form_tail(&message, usize::from(rect.width.saturating_sub(4)))
        } else {
            format!("⏎ {} · esc {}", dock.delete_label, dock.cancel_label)
        };
        write_endpoint_text(
            &mut cells,
            rect,
            4,
            2,
            &footer,
            StyleSpec {
                fg: if has_error {
                    error_color
                } else if removing {
                    accent
                } else {
                    subtext
                },
                bg: None,
                bold: removing,
            },
        );
        if let Some(MachinePopup::Confirm {
            rect: popup_rect,
            confirm_rect,
            cancel_rect,
            ..
        }) = dock.machine_popup.as_mut()
        {
            *popup_rect = Some(rect);
            *confirm_rect = (!removing).then_some(Rect::new(
                rect.x + 1,
                rect.y + 4,
                display_columns(dock.delete_label).saturating_add(3),
                1,
            ));
            *cancel_rect = (!removing).then_some(Rect::new(
                rect.x + display_columns(dock.delete_label).saturating_add(7),
                rect.y + 4,
                display_columns(dock.cancel_label).saturating_add(4),
                1,
            ));
        }
    }
    super::client::sync_begin();
    super::client::paint(terminal, &cells, *cursor, false, false, cursor)?;
    super::client::sync_end();
    dock.dirty = false;
    Ok(())
}

fn draw_popup_border(cells: &mut [(u16, u16, Cell)], rect: Rect, color: Color) {
    if rect.width < 2 || rect.height < 2 {
        return;
    }
    for column in 0..rect.width {
        set_form_symbol(cells, rect, column, 0, "─", color, false);
        set_form_symbol(cells, rect, column, rect.height - 1, "─", color, false);
    }
    for row in 0..rect.height {
        set_form_symbol(cells, rect, 0, row, "│", color, false);
        set_form_symbol(cells, rect, rect.width - 1, row, "│", color, false);
    }
    set_form_symbol(cells, rect, 0, 0, "┌", color, false);
    set_form_symbol(cells, rect, rect.width - 1, 0, "┐", color, false);
    set_form_symbol(cells, rect, 0, rect.height - 1, "└", color, false);
    set_form_symbol(
        cells,
        rect,
        rect.width - 1,
        rect.height - 1,
        "┘",
        color,
        false,
    );
}

fn machine_form_label_x(label: &str, label_width: u16) -> u16 {
    2u16.saturating_add(label_width.saturating_sub(display_columns(label)))
}

fn paint_machine_form(
    terminal: &mut DefaultTerminal,
    dock: &mut DockState,
    machines: &HashMap<String, MachineRuntime>,
    cursor: &mut Option<(u16, u16)>,
) -> Result<()> {
    let size = terminal.size()?;
    let mobile = size.width <= crate::app::MOBILE_WIDTH;
    let area = Rect::new(0, 0, size.width, size.height);
    let rect = dock
        .machine_form_rect
        .filter(|rect| {
            rect.width >= 2
                && rect.height >= 2
                && rect.right() <= area.right()
                && rect.bottom() <= area.bottom()
        })
        .unwrap_or_else(|| crate::ui::workspace_picker_modal_rect(area, mobile));
    dock.machine_form_rect = Some(rect);
    dock.machine_form_hits.clear();
    let theme = dock.form_theme;
    let surface = protocol::unpack(theme.surface);
    let border = protocol::unpack(theme.border);
    let text = protocol::unpack(theme.text);
    let subtext0 = protocol::unpack(theme.subtext0);
    let subtext1 = protocol::unpack(theme.subtext1);
    let accent = protocol::unpack(theme.accent);
    let accent_text = protocol::unpack(theme.accent_text);
    let divider = protocol::unpack(theme.divider);
    let rule = protocol::unpack(theme.rule);
    let error = protocol::unpack(theme.error);
    let mut cells = Vec::with_capacity(usize::from(rect.width) * usize::from(rect.height));
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            let mut cell = dock.base.clone();
            cell.set_symbol(" ");
            cell.set_fg(text);
            cell.set_bg(surface);
            cell.modifier = Modifier::empty();
            cells.push((x, y, cell));
        }
    }
    if rect.width >= 2 && rect.height >= 2 {
        for column in 0..rect.width {
            set_form_symbol(&mut cells, rect, column, 0, "─", border, false);
            set_form_symbol(
                &mut cells,
                rect,
                column,
                rect.height - 1,
                "─",
                border,
                false,
            );
        }
        for row in 0..rect.height {
            set_form_symbol(&mut cells, rect, 0, row, "│", border, false);
            set_form_symbol(&mut cells, rect, rect.width - 1, row, "│", border, false);
        }
        set_form_symbol(&mut cells, rect, 0, 0, "┌", border, false);
        set_form_symbol(&mut cells, rect, rect.width - 1, 0, "┐", border, false);
        set_form_symbol(&mut cells, rect, 0, rect.height - 1, "└", border, false);
        set_form_symbol(
            &mut cells,
            rect,
            rect.width - 1,
            rect.height - 1,
            "┘",
            border,
            false,
        );
    }

    let workspace = format!(" {} ", dock.workspace_label);
    let remote = format!(" {} ", dock.remote_machine_label);
    write_endpoint_text(
        &mut cells,
        rect,
        1,
        1,
        &workspace,
        StyleSpec {
            fg: subtext0,
            bg: None,
            bold: false,
        },
    );
    let remote_x = 2u16.saturating_add(display_columns(&workspace));
    write_endpoint_text(
        &mut cells,
        rect,
        1,
        remote_x,
        &remote,
        StyleSpec {
            fg: accent_text,
            bg: Some(accent),
            bold: true,
        },
    );
    dock.machine_form_hits.push((
        MachineFormHit::OpenWorkspaceTab,
        Rect::new(rect.x + 1, rect.y + 1, display_columns(&workspace), 1),
    ));
    dock.machine_form_hits.push((
        MachineFormHit::RemoteMachineTab,
        Rect::new(rect.x + remote_x, rect.y + 1, display_columns(&remote), 1),
    ));
    if rect.height > 3 {
        for column in 1..rect.width.saturating_sub(1) {
            set_form_symbol(&mut cells, rect, column, 3, "─", rule, false);
        }
    }

    let mut saved: Vec<_> = machines.values().collect();
    saved.sort_unstable_by(|a, b| {
        a.profile
            .label
            .cmp(&b.profile.label)
            .then_with(|| a.profile.id.cmp(&b.profile.id))
    });
    dock.saved_ids = if rect.height >= 19 {
        saved
            .iter()
            .map(|machine| machine.profile.id.clone())
            .collect()
    } else {
        Vec::new()
    };
    if let Some(form) = dock.machine_form.as_mut() {
        if let Some(id) = &form.selected_saved {
            if let Some(index) = dock.saved_ids.iter().position(|saved| saved == id) {
                form.cursor = form.fields.len() + index;
            } else {
                form.cursor = form.fields.len().saturating_sub(1);
                form.selected_saved = None;
            }
        }
    }
    let Some(form) = dock.machine_form.as_ref() else {
        return Ok(());
    };
    let labels = [
        format!("{}:", dock.machine_name_label),
        format!("{}:", dock.ssh_destination_label),
        format!("{}:", dock.session_name_label),
    ];
    let mut input_cursor = None;
    let label_width = labels
        .iter()
        .map(|label| display_columns(label))
        .max()
        .unwrap_or(0);
    for (field, (label, value)) in labels.iter().zip(form.fields.iter()).enumerate() {
        let row = 4 + field as u16 * 2;
        let label_x = machine_form_label_x(label, label_width);
        write_endpoint_text(
            &mut cells,
            rect,
            row,
            label_x,
            label,
            StyleSpec {
                fg: subtext0,
                bg: None,
                bold: false,
            },
        );
        let value_x = 4u16.saturating_add(label_width);
        let value_width = rect.width.saturating_sub(value_x + 2);
        let active = field == form.cursor;
        let (shown, caret_offset) = if active {
            machine_form_value_window(
                value,
                form.edit_cursors[field],
                usize::from(value_width.saturating_sub(1)),
            )
        } else {
            let shown = truncate_form_tail(value, usize::from(value_width.saturating_sub(1)));
            let width = display_columns(&shown);
            (shown, width)
        };
        write_endpoint_text(
            &mut cells,
            rect,
            row,
            value_x,
            &shown,
            StyleSpec {
                fg: if active { accent } else { text },
                bg: None,
                bold: active,
            },
        );
        if active && !form.submitting {
            let caret_x = value_x
                .saturating_add(caret_offset)
                .min(rect.width.saturating_sub(2));
            input_cursor = Some((rect.x + caret_x, rect.y + row));
        }
        dock.machine_form_hits.push((
            MachineFormHit::Field(field),
            Rect::new(rect.x + 1, rect.y + row, rect.width.saturating_sub(2), 1),
        ));
    }

    if let Some(reason) = form.install_prompt.as_deref() {
        let message = truncate_form_tail(reason, usize::from(rect.width.saturating_sub(5)));
        write_endpoint_text(
            &mut cells,
            rect,
            10,
            2,
            &message,
            StyleSpec {
                fg: error,
                bg: None,
                bold: false,
            },
        );
        let question = format!("{}?", dock.install_label);
        let question = truncate_form_tail(&question, usize::from(rect.width.saturating_sub(5)));
        write_endpoint_text(
            &mut cells,
            rect,
            11,
            2,
            &question,
            StyleSpec {
                fg: accent,
                bg: None,
                bold: true,
            },
        );
    }
    let footer_row = rect.height.saturating_sub(2);
    let divider_row = footer_row.saturating_sub(1);
    if form.install_prompt.is_none() && !dock.saved_ids.is_empty() {
        write_endpoint_text(
            &mut cells,
            rect,
            10,
            2,
            dock.saved_labels.0,
            StyleSpec {
                fg: subtext0,
                bg: None,
                bold: true,
            },
        );
        let capacity = usize::from(divider_row.saturating_sub(12) / 2);
        let selected = form
            .selected_saved
            .as_ref()
            .and_then(|id| dock.saved_ids.iter().position(|saved| saved == id));
        let start = selected.map_or(0, |index| index.saturating_add(1).saturating_sub(capacity));
        for (index, machine) in saved.iter().enumerate().skip(start).take(capacity) {
            let y = 12 + ((index - start) * 2) as u16;
            let selected = form.selected_saved.as_deref() == Some(machine.profile.id.as_str());
            let status = machine.runtime.status();
            let status_x = rect.width.saturating_sub(display_columns(status) + 2);
            let label = truncate_form_tail(
                &machine.profile.label,
                usize::from(status_x.saturating_sub(4)),
            );
            let style = StyleSpec {
                fg: if selected { accent } else { text },
                bg: None,
                bold: selected,
            };
            write_endpoint_text(&mut cells, rect, y, 2, &label, style);
            write_endpoint_text(
                &mut cells,
                rect,
                y,
                status_x,
                status,
                StyleSpec {
                    fg: subtext0,
                    bg: None,
                    bold: false,
                },
            );
            let host = truncate_form_tail(
                &machine.profile.destination,
                usize::from(rect.width.saturating_sub(6)),
            );
            write_endpoint_text(
                &mut cells,
                rect,
                y + 1,
                4,
                &host,
                StyleSpec {
                    fg: subtext0,
                    bg: None,
                    bold: false,
                },
            );
            dock.machine_form_hits.push((
                MachineFormHit::Saved(index),
                Rect::new(rect.x + 1, rect.y + y, rect.width.saturating_sub(2), 2),
            ));
        }
    }
    if divider_row > 3 {
        for column in 1..rect.width.saturating_sub(1) {
            set_form_symbol(&mut cells, rect, column, divider_row, "─", rule, false);
        }
    }
    if form.install_prompt.is_some() {
        let mut x = 1u16;
        let approve = format!("y {}", dock.install_action_label);
        write_endpoint_text(
            &mut cells,
            rect,
            footer_row,
            x,
            &approve,
            StyleSpec {
                fg: accent,
                bg: None,
                bold: true,
            },
        );
        dock.machine_form_hits.push((
            MachineFormHit::ApproveInstall,
            Rect::new(
                rect.x + x,
                rect.y + footer_row,
                display_columns(&approve),
                1,
            ),
        ));
        x = x.saturating_add(display_columns(&approve));
        let separator = " · ";
        write_endpoint_text(
            &mut cells,
            rect,
            footer_row,
            x,
            separator,
            StyleSpec {
                fg: divider,
                bg: None,
                bold: false,
            },
        );
        x = x.saturating_add(display_columns(separator));
        let decline = format!("n/↵/esc {}", dock.cancel_label);
        write_endpoint_text(
            &mut cells,
            rect,
            footer_row,
            x,
            &decline,
            StyleSpec {
                fg: subtext1,
                bg: None,
                bold: false,
            },
        );
        dock.machine_form_hits.push((
            MachineFormHit::DeclineInstall,
            Rect::new(
                rect.x + x,
                rect.y + footer_row,
                display_columns(&decline),
                1,
            ),
        ));
    } else if form.submitting {
        write_endpoint_text(
            &mut cells,
            rect,
            footer_row,
            1,
            dock.working_label,
            StyleSpec {
                fg: accent,
                bg: None,
                bold: true,
            },
        );
    } else if let Some(message) = form.error.as_deref() {
        let message = truncate_form_tail(message, usize::from(rect.width.saturating_sub(3)));
        write_endpoint_text(
            &mut cells,
            rect,
            footer_row,
            1,
            &message,
            StyleSpec {
                fg: error,
                bg: None,
                bold: false,
            },
        );
    } else {
        let mut x = 1u16;
        let submit_key = "⏎";
        write_endpoint_text(
            &mut cells,
            rect,
            footer_row,
            x,
            submit_key,
            StyleSpec {
                fg: accent,
                bg: None,
                bold: true,
            },
        );
        x = x.saturating_add(display_columns(submit_key));
        let action = form
            .selected_saved
            .as_ref()
            .and_then(|id| machines.get(id))
            .map_or(dock.create_label, |machine| {
                if machine.profile.enabled {
                    dock.saved_labels.1
                } else {
                    dock.saved_labels.2
                }
            });
        let submit_label = format!(" {}", action);
        write_endpoint_text(
            &mut cells,
            rect,
            footer_row,
            x,
            &submit_label,
            StyleSpec {
                fg: subtext1,
                bg: None,
                bold: false,
            },
        );
        let submit_width =
            display_columns(submit_key).saturating_add(display_columns(&submit_label));
        dock.machine_form_hits.push((
            MachineFormHit::Submit,
            Rect::new(rect.x + 1, rect.y + footer_row, submit_width, 1),
        ));
        x = x.saturating_add(display_columns(&submit_label));
        let separator = " · ";
        write_endpoint_text(
            &mut cells,
            rect,
            footer_row,
            x,
            separator,
            StyleSpec {
                fg: divider,
                bg: None,
                bold: false,
            },
        );
        x = x.saturating_add(display_columns(separator));
        let cancel_key = "esc";
        write_endpoint_text(
            &mut cells,
            rect,
            footer_row,
            x,
            cancel_key,
            StyleSpec {
                fg: accent,
                bg: None,
                bold: true,
            },
        );
        let cancel_x = x;
        x = x.saturating_add(display_columns(cancel_key));
        let cancel_label = format!(" {}", dock.cancel_label);
        write_endpoint_text(
            &mut cells,
            rect,
            footer_row,
            x,
            &cancel_label,
            StyleSpec {
                fg: subtext1,
                bg: None,
                bold: false,
            },
        );
        dock.machine_form_hits.push((
            MachineFormHit::Cancel,
            Rect::new(
                rect.x + cancel_x,
                rect.y + footer_row,
                display_columns(cancel_key).saturating_add(display_columns(&cancel_label)),
                1,
            ),
        ));
    }
    dock.machine_form_hits.push((MachineFormHit::Modal, rect));
    super::client::sync_begin();
    super::client::paint(
        terminal,
        &cells,
        input_cursor.or(*cursor),
        input_cursor.is_some(),
        false,
        cursor,
    )?;
    super::client::sync_end();
    dock.dirty = false;
    Ok(())
}

fn set_form_symbol(
    cells: &mut [(u16, u16, Cell)],
    rect: Rect,
    column: u16,
    row: u16,
    symbol: &str,
    color: Color,
    bold: bool,
) {
    let index = usize::from(row)
        .saturating_mul(usize::from(rect.width))
        .saturating_add(usize::from(column));
    if let Some((_, _, cell)) = cells.get_mut(index) {
        cell.set_symbol(symbol);
        cell.set_fg(color);
        if bold {
            cell.modifier.insert(Modifier::BOLD);
        }
    }
}

fn display_columns(text: &str) -> u16 {
    u16::try_from(unicode_width::UnicodeWidthStr::width(text)).unwrap_or(u16::MAX)
}

fn truncate_form_tail(text: &str, max: usize) -> String {
    if unicode_width::UnicodeWidthStr::width(text) <= max {
        return text.to_string();
    }
    let mut width = 0;
    let mut tail = Vec::new();
    for character in text.chars().rev() {
        let char_width = unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
        if width + char_width > max.saturating_sub(1) {
            break;
        }
        width += char_width;
        tail.push(character);
    }
    format!("…{}", tail.into_iter().rev().collect::<String>())
}

fn machine_form_value_window(text: &str, cursor: usize, max_width: usize) -> (String, u16) {
    if max_width == 0 {
        return (String::new(), 0);
    }
    let chars = text.chars().collect::<Vec<_>>();
    let cursor = cursor.min(chars.len());
    let widths = chars
        .iter()
        .map(|character| unicode_width::UnicodeWidthChar::width(*character).unwrap_or(0))
        .collect::<Vec<_>>();
    let prefix_width = widths[..cursor].iter().sum::<usize>();
    let start = if prefix_width <= max_width {
        0
    } else {
        let mut start = cursor;
        let mut width = 0;
        while start > 0 && width + widths[start - 1] <= max_width {
            start -= 1;
            width += widths[start];
        }
        start
    };
    let caret_offset = widths[start..cursor].iter().sum::<usize>();
    let mut end = cursor;
    let mut visible_width = caret_offset;
    while end < chars.len() && visible_width + widths[end] <= max_width {
        visible_width += widths[end];
        end += 1;
    }
    (
        chars[start..end].iter().collect(),
        u16::try_from(caret_offset).unwrap_or(u16::MAX),
    )
}

#[derive(Clone, Copy)]
struct StyleSpec {
    fg: Color,
    bg: Option<Color>,
    bold: bool,
}

fn write_endpoint_text(
    cells: &mut [(u16, u16, Cell)],
    rect: Rect,
    row: u16,
    indent: u16,
    text: &str,
    style: StyleSpec,
) {
    if row >= rect.height || indent >= rect.width {
        return;
    }
    let mut column = indent;
    for ch in text.chars() {
        let width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
        if width == 0 {
            continue;
        }
        if column.saturating_add(width) > rect.width {
            break;
        }
        let index = usize::from(row)
            .saturating_mul(usize::from(rect.width))
            .saturating_add(usize::from(column));
        let Some((_, _, cell)) = cells.get_mut(index) else {
            break;
        };
        cell.set_symbol(&ch.to_string());
        cell.set_fg(style.fg);
        if let Some(bg) = style.bg {
            cell.set_bg(bg);
        }
        if style.bold {
            cell.modifier.insert(Modifier::BOLD);
        }
        if width == 2 {
            if let Some((_, _, continuation)) = cells.get_mut(index + 1) {
                continuation.set_symbol("");
            }
        }
        column = column.saturating_add(width);
    }
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
        rect,
        0,
        &dock.heading.to_uppercase(),
        &selector_status,
        &dock.base,
        Color::Reset,
    );
    let visible_endpoints = usize::from(rect.height.saturating_sub(2) / row_height).max(1);
    let first_endpoint = dock
        .selector_cursor
        .saturating_add(1)
        .saturating_sub(visible_endpoints);
    for (visible_index, (index, endpoint)) in endpoints
        .iter()
        .enumerate()
        .skip(first_endpoint)
        .take(visible_endpoints)
        .enumerate()
    {
        let row = 2 + visible_index as u16 * row_height;
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
                let runtime = machine.endpoint(session);
                let state = runtime
                    .map(|runtime| &runtime.state)
                    .unwrap_or(&MachineState::Disabled);
                let tone = match state {
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
                    runtime.map_or("disabled", SessionRuntime::status),
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
            rect,
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
                rect,
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
            DockHit::Endpoint(endpoint.clone()),
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
    rect: Rect,
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
        if x >= rect.x.saturating_add(rect.width) {
            break;
        }
        // Both dock painters build `cells` in rect-relative row-major order.
        let index = usize::from(row)
            .saturating_mul(usize::from(rect.width))
            .saturating_add(usize::from(x - rect.x));
        if let Some((_, _, cell)) = cells.get_mut(index) {
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
    fn frame_diff_repaints_only_when_it_touches_the_client_dock() {
        let dock = Rect::new(0, 2, 24, 12);
        let diff = |start, cells| protocol::FrameDiff {
            width: 100,
            height: 30,
            runs: vec![protocol::DiffRun {
                start,
                fg: 0,
                bg: 0,
                mods: 0,
                symbols: vec![" ".to_string(); cells],
            }],
            cursor: None,
            cursor_visible: false,
        };
        assert!(frame_diff_intersects_rect(&diff(205, 4), dock));
        assert!(frame_diff_intersects_rect(&diff(195, 10), dock));
        assert!(!frame_diff_intersects_rect(&diff(250, 20), dock));
        assert!(!frame_diff_intersects_rect(&diff(1_500, 20), dock));
    }

    #[test]
    fn machine_form_labels_align_their_colons() {
        let labels = ["Machine name:", "SSH host:", "Session name:"];
        let width = labels
            .iter()
            .map(|label| display_columns(label))
            .max()
            .unwrap();
        let colon_columns = labels.map(|label| {
            machine_form_label_x(label, width)
                .saturating_add(display_columns(label))
                .saturating_sub(1)
        });
        assert!(colon_columns
            .windows(2)
            .all(|columns| columns[0] == columns[1]));
    }

    #[test]
    fn machine_catalog_notifications_are_revision_fenced() {
        let mut dock = DockState::default();
        assert!(note_catalog_revision(&mut dock, 5));
        assert_eq!(dock.catalog_revision, 5);
        assert!(dock.refresh_catalog);

        dock.refresh_catalog = false;
        assert!(!note_catalog_revision(&mut dock, 5));
        assert!(!note_catalog_revision(&mut dock, 4));
        assert!(!dock.refresh_catalog);
        assert!(note_catalog_revision(&mut dock, 6));
        assert!(dock.refresh_catalog);
    }

    #[test]
    fn machine_connection_menu_tracks_runtime_not_saved_enabled_flag() {
        assert!(!machine_can_disconnect(&MachineState::Disabled));
        assert!(!machine_can_disconnect(&MachineState::Attention(
            "offline".into()
        )));
        assert!(machine_can_disconnect(&MachineState::Online));
        assert!(machine_can_disconnect(&MachineState::Connecting {
            deadline: Instant::now()
        }));
        assert!(machine_can_disconnect(&MachineState::Reconnecting {
            at: Instant::now()
        }));
    }

    #[test]
    fn machine_menu_accepts_native_keyboard_navigation() {
        for code in [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Tab,
            KeyCode::BackTab,
            KeyCode::Char('j'),
            KeyCode::Char('k'),
        ] {
            let mut selected = 0;
            assert!(move_machine_popup_selection(&mut selected, &code));
            assert_eq!(selected, 1);
            assert!(move_machine_popup_selection(&mut selected, &code));
            assert_eq!(selected, 0);
        }

        let mut selected = 0;
        assert!(!move_machine_popup_selection(
            &mut selected,
            &KeyCode::Enter
        ));
        assert_eq!(selected, 0);
    }

    #[test]
    fn machine_disconnect_fences_events_and_stops_retry_without_clearing_workspaces() {
        let mut runtime = new_session_runtime(true, next_connection_generation());
        let generation = runtime.generation;
        disconnect_machine_runtime(&mut runtime);
        assert_ne!(runtime.generation, generation);
        assert!(matches!(runtime.state, MachineState::Disabled));
        assert!(runtime.control.is_none());
        assert!(runtime.reader.is_none());
    }

    #[test]
    fn saved_machine_navigation_preserves_fields_and_uses_catalog_identity() {
        let mut dock = DockState {
            machine_form: Some(MachineForm::default()),
            saved_ids: vec!["first".into(), "second".into()],
            ..DockState::default()
        };
        append_machine_form_text(&mut dock, "unfinished name");
        move_machine_form_cursor(&mut dock, 3);
        assert_eq!(
            dock.machine_form
                .as_ref()
                .unwrap()
                .selected_saved
                .as_deref(),
            Some("first")
        );
        append_machine_form_text(&mut dock, "must not edit a saved row");
        assert_eq!(
            dock.machine_form.as_ref().unwrap().fields[0],
            "unfinished name"
        );
        move_machine_form_cursor(&mut dock, 1);
        assert_eq!(
            dock.machine_form
                .as_ref()
                .unwrap()
                .selected_saved
                .as_deref(),
            Some("second")
        );
        move_machine_form_cursor(&mut dock, -4);
        assert!(dock.machine_form.as_ref().unwrap().selected_saved.is_none());
        assert_eq!(
            dock.machine_form.as_ref().unwrap().fields[0],
            "unfinished name"
        );
    }

    #[test]
    fn machine_install_prompt_is_explicit_and_defaults_to_cancel() {
        for code in [KeyCode::Char('y'), KeyCode::Char('Y')] {
            assert_eq!(install_prompt_choice(&code), Some(true));
        }
        for code in [
            KeyCode::Char('n'),
            KeyCode::Char('N'),
            KeyCode::Enter,
            KeyCode::Esc,
        ] {
            assert_eq!(install_prompt_choice(&code), Some(false));
        }
        assert_eq!(install_prompt_choice(&KeyCode::Tab), None);
    }

    #[test]
    fn saved_machine_open_reuses_existing_endpoint_without_catalog_mutation() {
        let mut dock = DockState {
            machine_form: Some(MachineForm {
                selected_saved: Some("box".into()),
                ..MachineForm::default()
            }),
            ..DockState::default()
        };
        let machine = machine_with_sessions("box", &["default"], MachineState::Online);
        let mut machines = HashMap::from([("box".into(), machine)]);
        let (tx, _rx) = mpsc::sync_channel(1);
        let mut pending = None;
        activate_machine_form(&mut dock, &tx, &mut pending, &mut machines, false).unwrap();
        assert_eq!(machines.len(), 1);
        assert_eq!(
            pending,
            Some(Endpoint::Remote {
                machine_id: "box".into(),
                session: "default".into()
            })
        );
        assert!(dock.machine_form.as_ref().unwrap().submitting);
        machines.clear();
        activate_machine_form(&mut dock, &tx, &mut pending, &mut machines, false).unwrap();
        assert!(dock.machine_form.as_ref().unwrap().error.is_some());
    }

    #[test]
    fn workspace_drag_width_obeys_both_native_seams_and_bounds() {
        let mut seam = protocol::ShellResize {
            left: true,
            column: 25,
            top: 2,
            bottom: 29,
            origin: 0,
            maximum: 44,
        };
        assert_eq!(resized_workspace_width(seam, 35), 36);
        assert_eq!(resized_workspace_width(seam, 200), 44);
        seam.left = false;
        seam.origin = 120;
        assert_eq!(resized_workspace_width(seam, 84), 36);
        assert_eq!(
            resized_workspace_width(seam, 120),
            crate::app::SIDEBAR_WIDTH_MIN
        );
    }

    #[test]
    fn remote_sidebar_drag_does_not_paint_ahead_of_the_endpoint_frame() {
        let mut dock = DockState::default();
        let theme = dock.form_theme;
        let resize = protocol::ShellResize {
            left: true,
            column: 25,
            top: 2,
            bottom: 29,
            origin: 0,
            maximum: 44,
        };
        dock.width_drag = Some(resize);
        dock.sidebars = Some(protocol::ShellSidebars {
            revision: 7,
            layout: crate::config::Config::default().sidebars(),
            workspace_paths: true,
        });
        dock.rect = Some(ShellDockRect {
            resize: Some(resize),
            session_slot: None,
            session_button: None,
            overlay: None,
            overlay_dims_background: false,
            workspace_focused: false,
            workspace_modal: false,
            x: 0,
            y: 2,
            width: 26,
            height: 27,
            show_paths: true,
            normal_fg: 0,
            secondary_fg: 0,
            active_fg: 0,
            active_secondary_fg: 0,
            active_bg: 0,
            branch_fg: 0,
            chrome: theme,
        });
        dock.dirty = false;

        assert!(update_client_sidebar_drag(&mut dock, 35));
        let state = dock.sidebars.as_ref().unwrap();
        assert_eq!(state.revision, 8);
        assert_eq!(state.layout.left.width, 36);
        assert_eq!(dock.workspace_width, None);
        let rect = dock.rect.unwrap();
        assert_eq!((rect.x, rect.width), (0, 26));
        assert_eq!(rect.resize.unwrap().column, 25);
        assert!(!dock.dirty);
    }

    #[test]
    fn rendered_sidebar_geometry_acknowledges_only_the_matching_width() {
        let mut state = protocol::ShellSidebars {
            revision: 8,
            layout: crate::config::Config::default().sidebars(),
            workspace_paths: true,
        };
        state.layout.left.width = 36;
        let resize = protocol::ShellResize {
            left: true,
            column: 35,
            top: 2,
            bottom: 29,
            origin: 0,
            maximum: 44,
        };
        let rect = ShellDockRect {
            resize: Some(resize),
            session_slot: None,
            session_button: None,
            overlay: None,
            overlay_dims_background: false,
            workspace_focused: false,
            workspace_modal: false,
            x: 0,
            y: 2,
            width: 36,
            height: 27,
            show_paths: true,
            normal_fg: 0,
            secondary_fg: 0,
            active_fg: 0,
            active_secondary_fg: 0,
            active_bg: 0,
            branch_fg: 0,
            chrome: DockState::default().form_theme,
        };

        assert!(shell_rect_matches_sidebar_state(rect, &state));
        state.layout.left.width = 26;
        assert!(!shell_rect_matches_sidebar_state(rect, &state));
    }

    #[test]
    fn acknowledged_sidebar_revision_is_not_resent() {
        let mut dock = DockState {
            sidebars: Some(protocol::ShellSidebars {
                revision: 8,
                layout: crate::config::Config::default().sidebars(),
                workspace_paths: true,
            }),
            ..DockState::default()
        };

        assert!(!has_newer_sidebar_revision(&dock, 8));
        dock.sidebars.as_mut().unwrap().revision = 9;
        assert!(has_newer_sidebar_revision(&dock, 8));
    }

    #[test]
    fn owner_session_chrome_uses_the_active_minimum_width_slot() {
        let active_slot = protocol::ShellDockBlock {
            x: 5,
            y: 0,
            width: 14,
            height: 1,
        };
        let dock = DockState {
            local_session: "uxfix".into(),
            owner_session_slot: Some(Rect::new(5, 0, 24, 1)),
            owner_session_button: Some(Rect::new(5, 0, 7, 1)),
            rect: Some(ShellDockRect {
                resize: None,
                session_slot: Some(active_slot),
                session_button: None,
                overlay: None,
                overlay_dims_background: false,
                workspace_focused: false,
                workspace_modal: false,
                x: 0,
                y: 2,
                width: 26,
                height: 27,
                show_paths: true,
                normal_fg: 0,
                secondary_fg: 0,
                active_fg: 0,
                active_secondary_fg: 0,
                active_bg: 0,
                branch_fg: 0,
                chrome: DockState::default().form_theme,
            }),
            ..DockState::default()
        };

        let (slot, button, label) = owner_session_projection(&dock).unwrap();
        assert_eq!(slot, Rect::new(5, 0, 14, 1));
        assert_eq!(button, Rect::new(5, 0, 7, 1));
        assert_eq!(label, " uxfix ");
        assert_eq!(slot.right(), 19, "the adjacent Menu starts at column 19");
        assert!(button.right() <= slot.right());
    }

    #[test]
    fn shell_geometry_commits_atomically_with_the_next_frame() {
        let theme = DockState::default().form_theme;
        let resize = protocol::ShellResize {
            left: false,
            column: 84,
            top: 2,
            bottom: 29,
            origin: 120,
            maximum: 44,
        };
        let current = ShellDockRect {
            resize: Some(resize),
            session_slot: None,
            session_button: None,
            overlay: None,
            overlay_dims_background: false,
            workspace_focused: false,
            workspace_modal: false,
            x: 84,
            y: 2,
            width: 36,
            height: 27,
            show_paths: true,
            normal_fg: 0,
            secondary_fg: 0,
            active_fg: 0,
            active_secondary_fg: 0,
            active_bg: 0,
            branch_fg: 0,
            chrome: theme,
        };
        let next = ShellDockRect {
            x: 94,
            width: 26,
            resize: Some(protocol::ShellResize {
                column: 94,
                ..resize
            }),
            ..current
        };
        let mut dock = DockState {
            rect: Some(current),
            pending_rect: Some(Some(next)),
            dirty: false,
            ..DockState::default()
        };
        assert_eq!(dock.rect, Some(current));

        apply_pending_shell_rect(&mut dock);
        assert_eq!(dock.rect, Some(next));
        assert!(dock.pending_rect.is_none());
        assert!(dock.dirty);
    }

    #[test]
    fn active_highlight_moves_to_machine_only_when_collapsed() {
        assert!(!machine_header_active(true, false));
        assert!(machine_header_active(true, true));
        assert!(!machine_header_active(false, true));
        assert!(!machine_header_active(false, false));
    }

    #[test]
    fn pointer_activation_releases_stale_keyboard_selection() {
        let mut dock = DockState {
            navigation: Some(2),
            hover: Some(DockHit::LocalWorkspace(0)),
            dirty: false,
            ..DockState::default()
        };
        let remote = DockHit::RemoteWorkspace(
            Endpoint::Remote {
                machine_id: "box".into(),
                session: "default".into(),
            },
            0,
        );

        assert!(claim_pointer_navigation(&mut dock, Some(&remote)));
        assert!(dock.navigation.is_none());
        assert_eq!(dock.hover, Some(remote));
        assert!(dock.dirty);
    }

    fn machine_with_sessions(id: &str, sessions: &[&str], state: MachineState) -> MachineRuntime {
        let mut profile = MachineProfile::new(id.to_string(), id.to_string());
        profile.preferred_session = sessions.first().map(|session| (*session).to_string());
        profile.sessions = sessions
            .iter()
            .map(|session| (*session).to_string())
            .collect();
        let mut machine = new_machine_runtime(profile);
        machine.runtime.state = state;
        machine
    }

    #[test]
    fn machine_form_input_is_bounded_and_control_free() {
        let mut dock = DockState {
            machine_form: Some(MachineForm::default()),
            ..DockState::default()
        };
        append_machine_form_text(&mut dock, "Build\nServer");
        assert_eq!(dock.machine_form.as_ref().unwrap().fields[0], "BuildServer");
        move_machine_form_cursor(&mut dock, 1);
        append_machine_form_text(&mut dock, &"h".repeat(300));
        assert_eq!(
            dock.machine_form.as_ref().unwrap().fields[1]
                .chars()
                .count(),
            255
        );
    }

    #[test]
    fn machine_form_edits_at_unicode_cursor_and_supports_navigation() {
        let mut dock = DockState {
            machine_form: Some(MachineForm::default()),
            ..DockState::default()
        };
        append_machine_form_text(&mut dock, "ab界d");
        move_machine_form_edit_cursor(&mut dock, -2, KeyModifiers::NONE);
        append_machine_form_text(&mut dock, "X");
        assert_eq!(dock.machine_form.as_ref().unwrap().fields[0], "abX界d");

        delete_machine_form_text(&mut dock, false, KeyModifiers::NONE);
        assert_eq!(dock.machine_form.as_ref().unwrap().fields[0], "ab界d");
        delete_machine_form_text(&mut dock, true, KeyModifiers::NONE);
        assert_eq!(dock.machine_form.as_ref().unwrap().fields[0], "abd");

        move_machine_form_edit_cursor_to_edge(&mut dock, false);
        append_machine_form_text(&mut dock, "界");
        assert_eq!(dock.machine_form.as_ref().unwrap().fields[0], "界abd");
        move_machine_form_edit_cursor_to_edge(&mut dock, true);
        assert_eq!(dock.machine_form.as_ref().unwrap().edit_cursors[0], 4);
    }

    #[test]
    fn machine_form_modified_delete_uses_word_and_line_boundaries() {
        let mut dock = DockState {
            machine_form: Some(MachineForm::default()),
            ..DockState::default()
        };
        append_machine_form_text(&mut dock, "Build server alpha");
        delete_machine_form_text(&mut dock, false, KeyModifiers::ALT);
        assert_eq!(
            dock.machine_form.as_ref().unwrap().fields[0],
            "Build server "
        );
        delete_machine_form_text(&mut dock, false, KeyModifiers::CONTROL);
        assert_eq!(dock.machine_form.as_ref().unwrap().fields[0], "Build ");
        delete_machine_form_text(&mut dock, false, KeyModifiers::SUPER);
        assert_eq!(dock.machine_form.as_ref().unwrap().fields[0], "");

        append_machine_form_text(&mut dock, "one two three");
        move_machine_form_edit_cursor_to_edge(&mut dock, false);
        delete_machine_form_text(&mut dock, true, KeyModifiers::CONTROL);
        assert_eq!(dock.machine_form.as_ref().unwrap().fields[0], " two three");
        delete_machine_form_text(&mut dock, true, KeyModifiers::SUPER);
        assert_eq!(dock.machine_form.as_ref().unwrap().fields[0], "");
    }

    #[test]
    fn machine_form_value_window_keeps_cursor_visible_on_long_wide_text() {
        assert_eq!(
            machine_form_value_window("alpha界beta", 0, 5),
            ("alpha".into(), 0)
        );
        assert_eq!(
            machine_form_value_window("alpha界beta", 6, 5),
            ("pha界".into(), 5)
        );
        assert_eq!(
            machine_form_value_window("alpha界beta", 10, 5),
            ("beta".into(), 4)
        );
    }

    #[test]
    fn form_tail_truncation_preserves_wide_character_boundaries() {
        assert_eq!(truncate_form_tail("alpha界", 5), "…ha界");
        assert_eq!(display_columns(&truncate_form_tail("alpha界", 5)), 5);
    }

    #[test]
    fn reconnect_backoff_is_bounded_and_jittered_per_endpoint() {
        let base = Duration::from_secs(10);
        let first = reconnect_delay("alpha:default", 1, base);
        let second = reconnect_delay("alpha:review", 1, base);
        assert!((Duration::from_secs(8)..=Duration::from_secs(12)).contains(&first));
        assert!((Duration::from_secs(8)..=Duration::from_secs(12)).contains(&second));
        assert_ne!(first, second);
    }

    #[test]
    fn disabled_endpoints_have_no_runtime_deadline() {
        let machine = machine_with_sessions("box", &["default"], MachineState::Disabled);
        let machines = HashMap::from([("box".to_string(), machine)]);
        assert!(next_deadline(&machines, None).is_none());
    }

    #[test]
    fn quiet_online_endpoint_schedules_one_health_deadline() {
        let mut machine = machine_with_sessions("box", &["default"], MachineState::Online);
        let evidence = Instant::now();
        machine.runtime.last_evidence = evidence;
        let machines = HashMap::from([("box".to_string(), machine)]);
        let deadline = next_deadline(&machines, None).expect("online health deadline");
        assert_eq!(deadline, evidence + LINK_HEALTH_IDLE);
    }

    #[test]
    fn handshake_timeouts_exhaust_budget_without_idle_retries() {
        let machine = machine_with_sessions("box", &["default"], MachineState::Disabled);
        let mut machines = HashMap::from([("box".to_string(), machine)]);
        for attempt in 1..=3 {
            let old_generation = machines["box"].runtime.generation;
            machines.get_mut("box").unwrap().runtime.state = MachineState::Connecting {
                deadline: Instant::now(),
            };
            expire_connecting(&mut machines);
            let runtime = &machines["box"].runtime;
            assert_ne!(runtime.generation, old_generation);
            assert_eq!(runtime.handshake_failures, attempt);
            if attempt < 3 {
                assert!(matches!(runtime.state, MachineState::Reconnecting { .. }));
            } else {
                assert!(matches!(runtime.state, MachineState::Attention(_)));
                assert!(next_deadline(&machines, None).is_none());
            }
        }
        expire_connecting(&mut machines);
        assert_eq!(machines["box"].runtime.handshake_failures, 3);
        assert!(matches!(
            machines["box"].runtime.state,
            MachineState::Attention(_)
        ));
    }

    #[test]
    fn handshake_eof_uses_the_same_bounded_retry_budget_as_timeout() {
        let mut runtime = new_session_runtime(true, next_connection_generation());
        for attempt in 1..=3 {
            runtime.state = MachineState::Connecting {
                deadline: Instant::now() + Duration::from_secs(10),
            };
            transition_disconnected_endpoint(
                "box",
                "default",
                &mut runtime,
                true,
                "SSH connection closed".into(),
                Instant::now(),
            );
            assert_eq!(runtime.handshake_failures, attempt);
            if attempt < 3 {
                assert!(matches!(runtime.state, MachineState::Reconnecting { .. }));
            } else {
                assert!(matches!(runtime.state, MachineState::Attention(_)));
            }
        }
    }

    #[test]
    fn protocol_failure_and_disabled_machine_never_enter_retry_loop() {
        let mut corrupt = new_session_runtime(true, next_connection_generation());
        corrupt.state = MachineState::Online;
        transition_disconnected_endpoint(
            "box",
            "default",
            &mut corrupt,
            true,
            "remote session protocol failed".into(),
            Instant::now(),
        );
        assert!(matches!(corrupt.state, MachineState::Attention(_)));

        let mut disabled = new_session_runtime(true, next_connection_generation());
        disabled.state = MachineState::Online;
        transition_disconnected_endpoint(
            "box",
            "default",
            &mut disabled,
            false,
            "SSH connection closed".into(),
            Instant::now(),
        );
        assert!(matches!(disabled.state, MachineState::Disabled));
    }

    #[test]
    fn one_endpoint_disconnect_does_not_mutate_a_sibling_runtime() {
        let now = Instant::now();
        let mut failed = new_session_runtime(true, next_connection_generation());
        failed.state = MachineState::Online;
        let mut sibling = new_session_runtime(true, next_connection_generation());
        sibling.state = MachineState::Online;
        sibling.boot_id = Some(42);
        sibling.last_evidence = now;

        transition_disconnected_endpoint(
            "alpha",
            "default",
            &mut failed,
            true,
            "SSH connection closed".into(),
            now,
        );

        assert!(matches!(failed.state, MachineState::Reconnecting { .. }));
        assert!(matches!(sibling.state, MachineState::Online));
        assert_eq!(sibling.boot_id, Some(42));
        assert_eq!(sibling.last_evidence, now);
    }

    #[test]
    fn workspace_reveal_tracks_keyboard_without_fighting_wheel_scroll() {
        let mut rows = vec![MachineDockRow::LocalHeading];
        rows.extend((0..10).map(|index| {
            MachineDockRow::LocalWorkspace(protocol::ShellWorkspace {
                id: format!("w{index}"),
                index,
                name: format!("project{index}"),
                cwd: "~/project".into(),
                branch: None,
                active: index == 0,
                selected: index == 9,
                nested: false,
                dot: "○".into(),
                dot_color: 0,
            })
        }));
        let mut dock = DockState::default();
        reveal_workspace(&rows, &Endpoint::Local, &mut dock, 6, true);
        assert_eq!(dock.scroll, 8);
        dock.scroll = 2;
        reveal_workspace(&rows, &Endpoint::Local, &mut dock, 6, true);
        assert_eq!(
            dock.scroll, 2,
            "unchanged selection must not undo wheel scrolling"
        );
        for row in &mut rows {
            if let MachineDockRow::LocalWorkspace(ws) = row {
                ws.selected = ws.index == 1;
            }
        }
        reveal_workspace(&rows, &Endpoint::Local, &mut dock, 6, true);
        assert_eq!(dock.scroll, 2);
        dock.scroll = 8;
        dock.revealed_workspace = None;
        reveal_workspace(&rows, &Endpoint::Local, &mut dock, 0, false);
        assert_eq!(dock.scroll, 2, "short layouts remain bounded");
    }

    #[test]
    fn one_machine_projects_workspaces_not_saved_session_rows() {
        let mut machine =
            machine_with_sessions("box", &["default", "review", "t03"], MachineState::Online);
        machine.runtime.workspaces = (0..11)
            .map(|index| protocol::ShellWorkspace {
                dot: "○".into(),
                dot_color: protocol::pack(Color::Gray),
                id: format!("workspace-{index}"),
                index,
                name: format!("project-{index}"),
                cwd: "/work/project".into(),
                branch: None,
                active: index == 0,
                selected: false,
                nested: false,
            })
            .collect();
        let machines = HashMap::from([("box".into(), machine)]);
        let rows = machine_dock_rows(&machines, &Endpoint::Local, &[]);
        assert_eq!(rows.len(), 13);
        assert!(matches!(&rows[0], MachineDockRow::LocalHeading));
        assert!(matches!(&rows[1], MachineDockRow::Machine(id) if id == "box"));
        assert!(rows[2..].iter().all(|row| matches!(row, MachineDockRow::RemoteWorkspace(Endpoint::Remote { session, .. }, _) if session == "default")));
        assert!(machines["box"].endpoint("review").is_none());
        assert_eq!(selector_endpoints(&machines).len(), 2);
        let mut keyboard = DockState {
            navigation: Some(12),
            navigation_reveal: true,
            ..DockState::default()
        };
        reveal_workspace(&rows, &Endpoint::Local, &mut keyboard, 3, false);
        assert_eq!(
            keyboard.scroll, 10,
            "Local focus can reach remote workspace rows"
        );
        keyboard.scroll = 2;
        reveal_workspace(&rows, &Endpoint::Local, &mut keyboard, 3, false);
        assert_eq!(
            keyboard.scroll, 2,
            "wheel scrolling is retained until another navigation key"
        );
        keyboard.navigation = Some(0);
        keyboard.navigation_reveal = true;
        reveal_workspace(&rows, &Endpoint::Local, &mut keyboard, 3, false);
        assert_eq!(keyboard.scroll, 0);
        assert_eq!(projected_dock_row_count(&machines, &Endpoint::Local, 0), 1);
        let mut dock = DockState::default();
        dock.collapsed_machines.insert("box".into());
        assert_eq!(
            visible_dock_rows(&machines, &Endpoint::Local, &dock).len(),
            2
        );
        assert_eq!(machines["box"].runtime.workspaces.len(), 11);
        assert!(!dock_layout(&Endpoint::Local, &HashMap::new(), 0).owns_workspaces);
        assert!(
            !dock_layout(&Endpoint::Local, &machines, 0).owns_session_chrome,
            "the local server keeps its native named-session control"
        );
        assert!(
            dock_layout(
                &Endpoint::Remote {
                    machine_id: "box".into(),
                    session: "default".into(),
                },
                &machines,
                0,
            )
            .owns_session_chrome,
            "remote servers leave session chrome to the owner-local client"
        );
    }

    #[test]
    fn legacy_session_list_does_not_create_runtime_endpoints() {
        let machine = machine_with_sessions(
            "box",
            &["default", "review", "t03", "t04"],
            MachineState::Online,
        );
        let machines = HashMap::from([("box".into(), machine)]);
        assert_eq!(
            selector_endpoints(&machines),
            vec![
                Endpoint::Local,
                Endpoint::Remote {
                    machine_id: "box".into(),
                    session: "default".into()
                }
            ]
        );
        assert!(machines["box"].endpoint("review").is_none());
        assert_eq!(dock_row_height(&MachineDockRow::LocalHeading, true), 1);
        assert_eq!(
            dock_row_height(&MachineDockRow::Machine("box".into()), true),
            2
        );
    }

    #[test]
    fn endpoint_identity_is_machine_and_session_without_channels() {
        let review = Endpoint::Remote {
            machine_id: "box".into(),
            session: "review".into(),
        };
        let same = Endpoint::Remote {
            machine_id: "box".into(),
            session: "review".into(),
        };
        let other = Endpoint::Remote {
            machine_id: "box".into(),
            session: "default".into(),
        };
        assert!(same_selection(&review, &same));
        assert!(!same_selection(&review, &other));
    }

    #[test]
    fn only_the_owner_local_endpoint_can_switch_named_sessions() {
        assert!(matches!(
            owner_local_session_switch(&Endpoint::Local, "review".into()),
            Some(super::super::client::ClientExit::SwitchSession(name)) if name == "review"
        ));
        assert!(
            owner_local_session_switch(
                &Endpoint::Remote {
                    machine_id: "box".into(),
                    session: "default".into(),
                },
                "remote-review".into(),
            )
            .is_none(),
            "a remote server cannot reinterpret its session menu as local navigation"
        );
    }

    #[test]
    fn endpoint_switches_use_active_native_geometry_over_stale_local_bounds() {
        let rect = |width, height| ShellDockRect {
            resize: None,
            session_slot: None,
            session_button: None,
            overlay: None,
            overlay_dims_background: false,
            workspace_focused: false,
            branch_fg: 0,
            workspace_modal: false,
            x: 0,
            y: 1,
            width,
            height,
            show_paths: false,
            normal_fg: 0,
            secondary_fg: 0,
            active_fg: 0,
            active_secondary_fg: 0,
            active_bg: 0,
            chrome: protocol::MachineFormTheme {
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
        };
        let owner = rect(28, 12);
        let mut remote = rect(40, 8);
        remote.show_paths = true;
        let endpoint = Endpoint::Remote {
            machine_id: "box".into(),
            session: "default".into(),
        };

        assert_eq!(active_shell_rect(Some(remote)), Some(remote));
        remote.show_paths = false;
        assert_eq!(
            active_shell_rect(Some(remote)),
            Some(remote),
            "remote Hide Path must not inherit Local settings"
        );
        assert_eq!(
            active_shell_rect(None),
            None,
            "an active compact endpoint must clear stale desktop dock geometry"
        );
        assert_eq!(
            candidate_shell_rect(&endpoint, None, Some(owner), Some(owner)),
            Some(owner)
        );
        assert_eq!(
            candidate_shell_rect(&Endpoint::Local, None, Some(owner), Some(owner)),
            Some(owner)
        );
        assert_eq!(active_shell_rect(None), None);
    }

    #[test]
    fn local_failover_replaces_only_the_pending_selection() {
        let mut candidate = Some(SurfaceCandidate {
            ticket: 1,
            endpoint: Endpoint::Remote {
                machine_id: "box".into(),
                session: "review".into(),
            },
            welcomed: true,
            ready: false,
            shell_dock: None,
            boot_id: Some(7),
            deadline: Instant::now() + Duration::from_secs(1),
        });
        replace_candidate_with_local(&mut candidate);
        assert!(matches!(
            candidate.as_ref().map(|candidate| &candidate.endpoint),
            Some(Endpoint::Local)
        ));
    }
}
