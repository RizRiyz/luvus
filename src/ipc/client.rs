//! Thin client (M2): connects to the server, forwards input, and blits the
//! frames it streams back onto the real terminal. Holds no app state.

use std::io::{BufReader, Read, Write};
use std::path::Path;
use std::thread;

use anyhow::{anyhow, Result};
use ratatui::backend::Backend;
use ratatui::buffer::Cell;
#[cfg(not(windows))]
use ratatui::crossterm::event::read as read_event;
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, Event,
};
use ratatui::crossterm::execute;
use ratatui::layout::Position;
use ratatui::{DefaultTerminal, Terminal};

use crate::ipc::protocol::{self, ClientMessage, FrameData, FrameDiff, ServerMessage};
use crate::ipc::transport;

#[derive(Debug)]
struct HandshakeIoError(std::io::Error);

impl std::fmt::Display for HandshakeIoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "connection failed before the Luvus handshake: {}",
            self.0
        )
    }
}

impl std::error::Error for HandshakeIoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

pub(crate) fn is_handshake_io_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<HandshakeIoError>().is_some()
}

fn read_handshake_message<R: Read>(reader: &mut R) -> Result<ServerMessage> {
    protocol::read_message(reader).map_err(|error| HandshakeIoError(error).into())
}

fn write_handshake_message<W: Write>(writer: &mut W, message: &ClientMessage) -> Result<()> {
    protocol::write_message(writer, message).map_err(|error| HandshakeIoError(error).into())
}

/// Attach to the local server over its Unix socket.
pub fn run(sock: &Path) -> Result<()> {
    let _logging = crate::logging::init(crate::logging::Role::Client);
    crate::logging::event(
        crate::logging::EventKind::ClientStart,
        &[crate::logging::Field::Role(crate::logging::Role::Client)],
    );
    let stream = match transport::connect(sock) {
        Ok(stream) => stream,
        Err(_) => {
            crate::logging::event(
                crate::logging::EventKind::ClientConnectFailed,
                &[crate::logging::Field::ErrorCode(
                    crate::logging::SafeId::new("io").expect("static id is valid"),
                )],
            );
            return Err(anyhow!("cannot connect to luvus server"));
        }
    };
    crate::logging::event(crate::logging::EventKind::ClientConnect, &[]);
    // `Conn` is a cloneable duplex handle: one clone reads, the other writes.
    attach_inner(stream.clone(), stream)
}

/// Attach a thin client over **any** reader/writer carrying the binary frame
/// protocol. The local path passes the two halves of a `Conn`; remote attach
/// (docs/18 RA) passes an `ssh` child's stdout/stdin — the protocol is the same.
pub fn attach<R, W>(reader: R, writer: W) -> Result<()>
where
    R: Read,
    W: Write + Send + 'static,
{
    let _logging = crate::logging::init(crate::logging::Role::Client);
    crate::logging::event(
        crate::logging::EventKind::ClientStart,
        &[crate::logging::Field::Role(crate::logging::Role::Client)],
    );
    crate::logging::event(crate::logging::EventKind::ClientConnect, &[]);
    attach_inner(reader, writer)
}

fn attach_inner<R, W>(reader: R, writer: W) -> Result<()>
where
    R: Read,
    W: Write + Send + 'static,
{
    let mut terminal = ratatui::init();
    crate::install_tui_panic_hook();
    let result = run_inner(reader, writer, &mut terminal);
    let _ = execute!(
        std::io::stdout(),
        crossterm::event::PopKeyboardEnhancementFlags,
        DisableFocusChange,
        DisableMouseCapture,
        DisableBracketedPaste
    );
    ratatui::restore();
    match result? {
        ClientExit::Done => Ok(()),
        ClientExit::Detached => {
            crate::print_detached_status(crate::i18n::cli::Context::configured());
            Ok(())
        }
        ClientExit::ServerStopped => {
            let context = crate::i18n::cli::Context::configured();
            let session = crate::session::display_name();
            let rows = [
                (context.text("status"), context.text("stopped")),
                (context.text("session"), session.as_str()),
            ];
            crate::cli::print_status_card("Luvus session", &rows);
            Ok(())
        }
        ClientExit::SwitchSession(name) => switch_session_process(&name),
    }
}

enum ClientExit {
    Done,
    Detached,
    ServerStopped,
    SwitchSession(String),
}

fn run_inner<R, W>(reader: R, mut writer: W, terminal: &mut DefaultTerminal) -> Result<ClientExit>
where
    R: Read,
    W: Write + Send + 'static,
{
    let mut host = HostTerminal {
        truecolor: protocol::truecolor_supported(),
        // Answered by the probe below, before any frame is painted.
        graphics: false,
    };
    let size = terminal.size()?;
    write_handshake_message(
        &mut writer,
        &ClientMessage::Hello {
            version: protocol::PROTOCOL_VERSION,
            cols: size.width,
            rows: size.height,
        },
    )?;

    let mut reader = BufReader::new(reader);
    match read_handshake_message(&mut reader)? {
        // The one user-facing handshake failure is an old server after an
        // upgrade — tell them the fix, not just the symptom.
        ServerMessage::Welcome { error: Some(e), .. } => {
            crate::logging::event(
                crate::logging::EventKind::ClientHandshakeRejected,
                &[
                    crate::logging::Field::Reason(crate::logging::Reason::VersionMismatch),
                    crate::logging::Field::ProtocolVersion(u64::from(protocol::PROTOCOL_VERSION)),
                ],
            );
            return Err(anyhow!(
                "server: {e}\nAn older luvus server is likely still running — \
                 run `luvus server restart` to load this version (your session is saved)."
            ));
        }
        ServerMessage::Welcome { .. } => {}
        _ => {
            crate::logging::event(
                crate::logging::EventKind::ClientHandshakeRejected,
                &[crate::logging::Field::Reason(
                    crate::logging::Reason::Handshake,
                )],
            );
            return Err(anyhow!("unexpected handshake"));
        }
    }

    let probe_colors = match read_handshake_message(&mut reader)? {
        ServerMessage::Ready { probe_colors } => probe_colors,
        _ => return Err(anyhow!("unexpected handshake negotiation")),
    };
    // The probe always runs. The server needs to know whether this terminal can
    // draw images no matter which theme is configured, and asking costs one
    // round trip that the palette query would otherwise pay for alone.
    let probe = crate::terminal::theme_probe::probe(probe_colors);
    host.graphics = probe.graphics.unwrap_or(false);
    protocol::write_message(
        &mut writer,
        &ClientMessage::TerminalProbe {
            colors: probe.colors,
            graphics: probe.graphics,
        },
    )?;
    let pending = probe.pending;
    crate::logging::event(
        crate::logging::EventKind::ClientHandshake,
        &[
            crate::logging::Field::ProtocolVersion(u64::from(protocol::PROTOCOL_VERSION)),
            crate::logging::Field::Cols(u64::from(size.width)),
            crate::logging::Field::Rows(u64::from(size.height)),
        ],
    );

    // Enable input protocols only after probing. That bounds the pending-input
    // decoder to ordinary terminal key sequences and avoids mouse/paste replies
    // becoming interleaved with OSC palette responses.
    let _ = execute!(
        std::io::stdout(),
        EnableBracketedPaste,
        EnableMouseCapture,
        // Focus reporting: regaining focus (e.g. after moving the window or tabbing
        // back) is our cue that the terminal may have been repainted underneath us,
        // so we ask the server for a full frame (see the input loop).
        EnableFocusChange,
        crossterm::terminal::SetTitle(crate::window_title())
    );
    #[cfg(windows)]
    let _windows_input_mode = crate::terminal::host_input::enable_input_mode();
    // Let the terminal report Shift+Enter et al. as distinct keys, so agents get
    // a real "new line" key instead of a bare CR (see `push_key_protocol`).
    crate::push_key_protocol();

    // Input thread: terminal events → the server.
    thread::spawn(move || input_loop(writer, pending));

    // Main thread: paint frames as they arrive. A full frame repaints the screen; a
    // diff writes only its changed cells straight to the terminal (no full re-blit,
    // no reconstructed frame) — so a busy session costs O(changed cells), not O(screen).
    // `last_cursor` parks IME when this frame hid the PTY caret: CUP onto the
    // pane even after `?25l`, so composition does not follow chrome.
    let mut last_cursor = None;
    let exit = loop {
        match protocol::read_message::<_, ServerMessage>(&mut reader) {
            // A full frame repaints the whole screen; a diff writes *only its changed
            // cells* straight to the terminal (O(changed), not a whole re-blit). Each
            // is wrapped in a DEC 2026 synchronized update so it paints atomically.
            Ok(ServerMessage::Frame(frame)) => {
                sync_begin();
                let r = paint(
                    terminal,
                    &frame_cells(&frame, host),
                    frame.cursor,
                    frame.cursor_visible,
                    true,
                    &mut last_cursor,
                );
                sync_end();
                if r.is_err() {
                    crate::logging::event(
                        crate::logging::EventKind::ClientRenderFailed,
                        &[crate::logging::Field::ErrorCode(
                            crate::logging::SafeId::new("io").expect("static id is valid"),
                        )],
                    );
                }
                r?;
            }
            Ok(ServerMessage::FrameDiff(diff)) => {
                sync_begin();
                let r = paint(
                    terminal,
                    &diff_cells(&diff, host),
                    diff.cursor,
                    diff.cursor_visible,
                    false,
                    &mut last_cursor,
                );
                sync_end();
                if r.is_err() {
                    crate::logging::event(
                        crate::logging::EventKind::ClientRenderFailed,
                        &[crate::logging::Field::ErrorCode(
                            crate::logging::SafeId::new("io").expect("static id is valid"),
                        )],
                    );
                }
                r?;
            }
            // Ahead of the frame whose placeholder cells refer to these, on the
            // same ordered channel, so the terminal always knows an image
            // before it is asked to draw one.
            Ok(ServerMessage::Graphics(commands)) => crate::emit_graphics(&commands),
            Ok(ServerMessage::Notify(msg)) => crate::emit_notification(&msg),
            Ok(ServerMessage::Sound(signal)) => crate::emit_sound(signal),
            Ok(ServerMessage::Clipboard(text)) => crate::emit_clipboard(&text),
            Ok(ServerMessage::OpenUrl(url)) => crate::platform::open_url(&url),
            Ok(ServerMessage::SwitchSession { name }) => break ClientExit::SwitchSession(name),
            Ok(ServerMessage::Detach) => break ClientExit::Detached,
            Ok(ServerMessage::ServerShutdown { .. }) => break ClientExit::ServerStopped,
            Ok(_) => {}
            Err(_) => {
                crate::logging::event(
                    crate::logging::EventKind::ClientFrameError,
                    &[crate::logging::Field::ErrorCode(
                        crate::logging::SafeId::new("protocol").expect("static id is valid"),
                    )],
                );
                crate::logging::event(
                    crate::logging::EventKind::ClientDisconnect,
                    &[crate::logging::Field::Reason(
                        crate::logging::Reason::Protocol,
                    )],
                );
                break ClientExit::Done;
            }
        }
    };
    Ok(exit)
}

/// Hand this thin client process to the same launch mode targeting another
/// logical session. Unix replaces the process. Windows starts the successor
/// and immediately lets this process exit, so the old terminal-input thread is
/// never left reading alongside the new client. Local launches and `--remote`
/// retain their existing arguments and SSH options.
fn switch_session_process(name: &str) -> Result<()> {
    crate::session::validate_name(name).map_err(anyhow::Error::msg)?;
    let raw: Vec<String> = std::env::args().collect();
    let args = switched_args(&raw, name);
    let exe = std::env::current_exe()?;
    let mut command = std::process::Command::new(exe);
    command.args(args).env_remove("LUVUS_SOCKET_PATH");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec().into())
    }
    #[cfg(not(unix))]
    {
        command.spawn()?;
        Ok(())
    }
}

fn switched_args(raw: &[String], name: &str) -> Vec<String> {
    let mut out = vec!["--session".to_string(), name.to_string()];
    let mut index = 1;
    while index < raw.len() {
        if raw[index] == "--session" {
            index = (index + 2).min(raw.len());
            continue;
        }
        if raw[index].starts_with("--session=") {
            index += 1;
            continue;
        }
        // Only replace an initial global selector. Later flags belong to the
        // command itself, for example `pane report --session <native-id>`.
        out.extend_from_slice(&raw[index..]);
        break;
    }
    out
}

fn input_loop<W: Write>(mut writer: W, pending: Vec<Event>) {
    #[cfg(windows)]
    {
        crate::terminal::host_input::run_input_loop(pending, |event| {
            write_input_event(&mut writer, event)
        });
    }

    #[cfg(not(windows))]
    {
        for event in pending {
            if !write_input_event(&mut writer, event) {
                return;
            }
        }
        while let Ok(event) = read_event() {
            if !write_input_event(&mut writer, event) {
                break;
            }
        }
    }
}

fn write_input_event(writer: &mut impl Write, event: Event) -> bool {
    event_message(event).is_none_or(|message| protocol::write_message(writer, &message).is_ok())
}

fn event_message(event: Event) -> Option<ClientMessage> {
    event_message_with_image(event, crate::platform::clipboard_image)
}

fn event_message_with_image(
    event: Event,
    image: impl FnOnce() -> Option<Vec<u8>>,
) -> Option<ClientMessage> {
    match crate::terminal::host_key::normalize_platform_modifiers(event) {
        Event::Key(k) if crate::clipboard_image::is_image_paste_key(&k) => image()
            .map(ClientMessage::ClipboardImage)
            .or(Some(ClientMessage::Key(k))),
        Event::Key(k) => Some(ClientMessage::Key(k)),
        Event::Mouse(m) => Some(ClientMessage::Mouse(m)),
        Event::Resize(cols, rows) => {
            crate::logging::event(
                crate::logging::EventKind::ClientResize,
                &[
                    crate::logging::Field::Cols(u64::from(cols)),
                    crate::logging::Field::Rows(u64::from(rows)),
                ],
            );
            Some(ClientMessage::Resize { cols, rows })
        }
        Event::Paste(s) => Some(ClientMessage::Paste(s)),
        // Regained focus: the window may have moved or been repainted while we
        // were away, and luvus never saw it. Re-send the current size, which the
        // server treats as a forced full repaint, healing any stale cells.
        Event::FocusGained => crossterm::terminal::size()
            .ok()
            .map(|(cols, rows)| ClientMessage::Resize { cols, rows }),
        _ => None,
    }
}

/// The remote-side bridge (docs/18 RA-1): connect to the local server socket and
/// relay it byte-for-byte to/from this process's stdin/stdout, which `ssh` has
/// wired back to the `luvus --remote` client. The binary frame protocol flows
/// over the pipe unchanged.
pub fn remote_bridge(sock: &Path) -> Result<()> {
    let conn = transport::connect(sock).map_err(|_| anyhow!("cannot connect to luvus server"))?;
    relay(conn.clone(), conn, std::io::stdin(), std::io::stdout())
}

/// Pump bytes both directions: `input → local_writer` (a background thread) and
/// `local_reader → output` (this thread). Returns when either side closes.
/// Protocol-agnostic — it copies and flushes each available chunk so a
/// long-lived SSH pipe cannot buffer interactive frames indefinitely.
pub fn relay<LR, LW, I, O>(
    local_reader: LR,
    local_writer: LW,
    input: I,
    mut output: O,
) -> Result<()>
where
    LR: Read,
    LW: Write + Send + 'static,
    I: Read + Send + 'static,
    O: Write,
{
    let mut local_writer = local_writer;
    let mut input = input;
    thread::spawn(move || {
        let _ = copy_and_flush(&mut input, &mut local_writer);
    });
    let mut local_reader = local_reader;
    copy_and_flush(&mut local_reader, &mut output)?;
    Ok(())
}

/// Copy a stream without adding user-space batching latency. `Read` may return
/// any protocol fragment, so flushing per read preserves byte transparency
/// while making every currently available chunk visible to the next hop.
fn copy_and_flush<R: Read, W: Write>(reader: &mut R, writer: &mut W) -> std::io::Result<u64> {
    let mut buf = [0u8; 16 * 1024];
    let mut copied = 0u64;

    loop {
        let read = match reader.read(&mut buf) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if read == 0 {
            writer.flush()?;
            return Ok(copied);
        }
        writer.write_all(&buf[..read])?;
        writer.flush()?;
        copied += read as u64;
    }
}

/// Begin/end a DEC 2026 synchronized update so a frame paints atomically (no
/// tearing). Terminals without it ignore the sequence.
fn sync_begin() {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(b"\x1b[?2026h");
    let _ = out.flush();
}
fn sync_end() {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(b"\x1b[?2026l");
    let _ = out.flush();
}

/// What this client's own terminal can do with a cell.
///
/// Every attached client sees the same frame, because the panes are shared, but
/// the terminals in front of each user are not the same. Both of these describe
/// the terminal this process is writing to, never the pane.
#[derive(Clone, Copy)]
struct HostTerminal {
    /// Whether 24-bit color reaches the terminal intact.
    truecolor: bool,
    /// Whether it draws kitty graphics.
    graphics: bool,
}

impl HostTerminal {
    /// A terminal with nothing in the way of what the server sends, so a test
    /// asserting on frame content sees the cells exactly as they arrived.
    #[cfg(test)]
    fn drawing() -> Self {
        Self {
            truecolor: true,
            graphics: true,
        }
    }
}

/// Build one ratatui `Cell` from wire fields (control chars → space; 256-color
/// downsampling on non-truecolor terminals).
fn make_cell(sym: &str, fg: u32, bg: u32, mods: u16, host: HostTerminal) -> Cell {
    // An image cell is not text. Where the terminal draws images it resolves
    // the cell against the image whose id the foreground carries, so both the
    // character and that id must survive exactly — downsampling the id to the
    // nearest 256-color entry would point the cell at a different image, or
    // none. Where the terminal draws no images there is nothing to resolve it
    // against, and printing the character would put a private-use glyph on
    // screen, so it is blanked instead.
    let image_cell = crate::terminal::graphics::placeholder::is_placeholder(sym);
    if image_cell && !host.graphics {
        let mut cell = Cell::default();
        cell.set_symbol(" ");
        cell.set_bg(if host.truecolor {
            protocol::unpack(bg)
        } else {
            protocol::to_256(protocol::unpack(bg))
        });
        return cell;
    }
    let exact = host.truecolor || image_cell;
    let adjust = |c| if exact { c } else { protocol::to_256(c) };
    // ratatui panics on control chars in a symbol; the server filters, but never
    // trust the wire. (Empty symbols are wide-char continuations and are already
    // skipped by `frame_cells`/`diff_cells`, so they never reach here.)
    let s = if sym.chars().any(|c| c.is_control()) {
        " "
    } else {
        sym
    };
    let mut cell = Cell::default();
    cell.set_symbol(s); // copies into the cell (no borrow), unlike `Cell::new`
    cell.set_fg(adjust(protocol::unpack(fg)));
    cell.set_bg(adjust(protocol::unpack(bg)));
    cell.modifier = protocol::unpack_mods(mods);
    cell
}

/// Every cell of a full frame as `(x, y, Cell)`.
fn frame_cells(frame: &FrameData, host: HostTerminal) -> Vec<(u16, u16, Cell)> {
    frame
        .cells
        .iter()
        .enumerate()
        // An empty symbol is a wide-char continuation (the cell right of a
        // double-width glyph — the renderer marks it so). It must NOT be drawn:
        // the glyph already covers that column, and blitting a space there would
        // overwrite the glyph's right half and shift the row. Skipping it also
        // makes the next real cell non-contiguous, so crossterm re-anchors with a
        // MoveTo — keeping the whole row aligned.
        .filter(|(_, c)| !c.symbol.is_empty())
        .map(|(i, c)| {
            let i = i as u16;
            (
                i % frame.width,
                i / frame.width,
                make_cell(&c.symbol, c.fg, c.bg, c.mods, host),
            )
        })
        .collect()
}

/// Only the changed cells of a diff as `(x, y, Cell)` — the whole point: O(changed).
fn diff_cells(diff: &FrameDiff, host: HostTerminal) -> Vec<(u16, u16, Cell)> {
    let w = diff.width as u32;
    let mut cells = Vec::new();
    for run in &diff.runs {
        for (k, sym) in run.symbols.iter().enumerate() {
            if sym.is_empty() {
                continue; // wide-char continuation — see `frame_cells`
            }
            let i = run.start + k as u32;
            cells.push((
                (i % w) as u16,
                (i / w) as u16,
                make_cell(sym, run.fg, run.bg, run.mods, host),
            ));
        }
    }
    cells
}

/// Visible in-bounds pane cell. Does not remap a compact/mobile row-0/1 caret
/// onto the status line, and does not invent a prompt row.
fn ime_position(cursor: Option<(u16, u16)>, tw: u16, th: u16) -> Option<(u16, u16)> {
    cursor.filter(|(x, y)| *x < tw && *y < th)
}

/// Write `cells` straight to the terminal via the backend (no full re-blit / no
/// ratatui double-buffer), position the cursor, and flush. `clear` first wipes the
/// screen (full frame / resync); diffs paint over what's already there.
///
/// Hide, write cells, CUP to the pane PTY (hidden still parks), then show/hide.
/// `backend.draw` walks the hardware cursor onto the last cell (e.g. a
/// `working` spinner); IME must not observe that cell.
fn paint<B>(
    terminal: &mut Terminal<B>,
    cells: &[(u16, u16, Cell)],
    cursor: Option<(u16, u16)>,
    cursor_visible: bool,
    clear: bool,
    last_cursor: &mut Option<(u16, u16)>,
) -> Result<()>
where
    B: Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    // Clamp to the terminal size so a resize race can't index out of bounds.
    let size = terminal.size()?;
    let (tw, th) = (size.width, size.height);
    let backend = terminal.backend_mut();
    backend.hide_cursor()?;
    if clear {
        backend.clear()?;
    }
    backend.draw(
        cells
            .iter()
            .filter(|(x, y, _)| *x < tw && *y < th)
            .map(|(x, y, c)| (*x, *y, c)),
    )?;
    match ime_position(cursor, tw, th) {
        Some((x, y)) => {
            *last_cursor = Some((x, y));
            backend.set_cursor_position(Position::new(x, y))?;
            if cursor_visible {
                backend.show_cursor()?;
            } else {
                backend.hide_cursor()?;
            }
        }
        None => {
            if let Some((x, y)) = *last_cursor {
                if x < tw && y < th {
                    backend.set_cursor_position(Position::new(x, y))?;
                }
            }
            backend.hide_cursor()?;
        }
    }
    backend.flush()?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::{
        copy_and_flush, is_handshake_io_error, read_handshake_message, relay,
        write_handshake_message,
    };
    use crate::ipc::protocol::{ClientMessage, PROTOCOL_VERSION};
    use std::cell::RefCell;
    use std::io::{Cursor, Read, Write};
    use std::os::unix::net::UnixStream;
    use std::rc::Rc;
    use std::thread;

    #[test]
    fn empty_remote_stream_is_classified_as_a_handshake_failure() {
        let error = match read_handshake_message(&mut Cursor::new(Vec::<u8>::new())) {
            Err(error) => error,
            Ok(_) => panic!("an empty stream must not complete the handshake"),
        };
        assert!(is_handshake_io_error(&error));
        assert_eq!(
            error.to_string(),
            "connection failed before the Luvus handshake: failed to fill whole buffer"
        );
    }

    #[test]
    fn closed_remote_input_is_classified_as_a_handshake_failure() {
        struct ClosedWriter;
        impl Write for ClosedWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "remote command exited",
                ))
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let error = write_handshake_message(
            &mut ClosedWriter,
            &ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                cols: 80,
                rows: 24,
            },
        )
        .unwrap_err();
        assert!(is_handshake_io_error(&error));
        assert_eq!(
            error.to_string(),
            "connection failed before the Luvus handshake: remote command exited"
        );
    }

    /// The blit skips wide-char continuation cells (empty symbol) instead of
    /// drawing a space into the glyph's right half — the emoji-glitch fix. The
    /// real char after the emoji stays at its column.
    #[test]
    fn blit_skips_wide_char_continuation() {
        use crate::ipc::protocol::{pack, CellData, FrameData};
        use ratatui::style::Color;
        let c = |symbol: &str| CellData {
            symbol: symbol.to_string(),
            fg: pack(Color::Reset),
            bg: pack(Color::Reset),
            mods: 0,
        };
        // Row: [🔴][continuation ""][A][B]
        let frame = FrameData {
            width: 4,
            height: 1,
            cells: vec![c("\u{1F534}"), c(""), c("A"), c("B")],
            cursor: None,
            cursor_visible: false,
        };
        let cells = super::frame_cells(&frame, super::HostTerminal::drawing());
        let syms: Vec<(u16, String)> = cells
            .iter()
            .map(|(x, _, cell)| (*x, cell.symbol().to_string()))
            .collect();
        // The continuation cell (x=1) is absent; the emoji, A, B keep their x.
        assert!(
            !syms.iter().any(|(x, _)| *x == 1),
            "continuation cell skipped"
        );
        assert!(syms.contains(&(0, "\u{1F534}".to_string())), "emoji at x=0");
        assert!(syms.contains(&(2, "A".to_string())), "A stays at x=2");
        assert!(syms.contains(&(3, "B".to_string())), "B stays at x=3");
    }

    #[test]
    fn relay_pumps_both_directions() {
        // `client_side` simulates the local server socket the bridge connects to;
        // `server_side` is the (fake) server on the other end.
        let (client_side, mut server_side) = UnixStream::pair().unwrap();
        let srv = thread::spawn(move || {
            let mut got = [0u8; 5];
            server_side.read_exact(&mut got).unwrap(); // the forwarded input
            server_side.write_all(b"world").unwrap(); // the reply
            got // drop server_side after → client read EOFs, relay returns
        });

        let reader = client_side.try_clone().unwrap();
        let mut output: Vec<u8> = Vec::new();
        relay(
            reader,
            client_side,
            Cursor::new(b"hello".to_vec()),
            &mut output,
        )
        .unwrap();

        assert_eq!(&srv.join().unwrap(), b"hello", "input forwarded to server");
        assert_eq!(output, b"world", "server reply forwarded to output");
    }

    #[test]
    fn streaming_copy_flushes_each_chunk_and_retries_interrupts() {
        #[derive(Default)]
        struct WriterState {
            pending: Vec<u8>,
            visible: Vec<u8>,
            flushes: usize,
        }

        struct BufferedWriter(Rc<RefCell<WriterState>>);

        impl Write for BufferedWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.borrow_mut().pending.extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                let mut state = self.0.borrow_mut();
                let pending = std::mem::take(&mut state.pending);
                state.visible.extend_from_slice(&pending);
                state.flushes += 1;
                Ok(())
            }
        }

        struct ControlledReader {
            step: u8,
            writer: Rc<RefCell<WriterState>>,
        }

        impl Read for ControlledReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let chunk = match self.step {
                    0 => b"interactive ".as_slice(),
                    1 => {
                        let state = self.writer.borrow();
                        assert_eq!(state.visible, b"interactive ");
                        assert_eq!(state.flushes, 1, "first chunk flushed before next read");
                        self.step += 1;
                        return Err(std::io::ErrorKind::Interrupted.into());
                    }
                    2 => b"frame".as_slice(),
                    3 => {
                        let state = self.writer.borrow();
                        assert_eq!(state.visible, b"interactive frame");
                        assert_eq!(state.flushes, 2, "second chunk flushed before EOF");
                        self.step += 1;
                        return Ok(0);
                    }
                    _ => return Ok(0),
                };
                buf[..chunk.len()].copy_from_slice(chunk);
                self.step += 1;
                Ok(chunk.len())
            }
        }

        let state = Rc::new(RefCell::new(WriterState::default()));
        let mut reader = ControlledReader {
            step: 0,
            writer: Rc::clone(&state),
        };
        let mut writer = BufferedWriter(Rc::clone(&state));
        let copied = copy_and_flush(&mut reader, &mut writer).unwrap();

        assert_eq!(copied, 17);
        let state = state.borrow();
        assert_eq!(state.visible, b"interactive frame");
        assert!(state.pending.is_empty());
        assert_eq!(state.flushes, 3, "two chunk flushes plus the EOF flush");
    }

    /// A real scratch server must negotiate a client-owned terminal palette and
    /// return a frame. The byte-transparent bridge is covered separately by
    /// `relay_pumps_both_directions`.
    /// This remains Unix-only because the surrounding relay tests use
    /// `UnixStream`. A filtered copy of the current test executable runs the
    /// real server, so this works in a clean target directory without requiring
    /// a separate `cargo build` first.
    #[test]
    fn real_server_accepts_a_terminal_palette() {
        use crate::ipc::protocol::{self, ClientMessage, ServerMessage, PROTOCOL_VERSION};
        use std::process::{Command, Stdio};

        let bin = std::env::current_exe().unwrap();
        let home = std::env::temp_dir().join(format!("luvus-remote-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let config = crate::config::Config {
            theme: "terminal".into(),
            shell: "/bin/sh".into(),
            ..Default::default()
        };
        std::fs::write(
            home.join("config.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();

        // Keep startup diagnostics out of pipes, which can fill and block the
        // child. Only a bounded excerpt is read if startup fails.
        let diagnostics = std::fs::File::create(home.join("startup.log")).unwrap();
        // A real server on a scratch home.
        let server = Command::new(&bin)
            .args([
                "--exact",
                "ipc::client::tests::terminal_palette_server_helper",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("LUVUS_TEST_PALETTE_SERVER", "1")
            .env("LUVUS_HOME", &home)
            // An agent pane inherits the live session's socket. The scratch
            // server and its cleanup must never escape this test home.
            .env_remove("LUVUS_SOCKET_PATH")
            .env_remove("LUVUS_SESSION")
            .env_remove("LUVUS_PANE_ID")
            .env_remove("LUVUS_TASK_ID")
            .stdin(Stdio::null())
            .stdout(diagnostics.try_clone().unwrap())
            .stderr(diagnostics)
            .spawn()
            .unwrap();
        struct ScratchServer {
            child: std::process::Child,
            home: std::path::PathBuf,
        }
        impl Drop for ScratchServer {
            fn drop(&mut self) {
                let _ = self.child.kill();
                let _ = self.child.wait();
                let _ = std::fs::remove_dir_all(&self.home);
            }
        }
        let mut server = ScratchServer {
            child: server,
            home: home.clone(),
        };
        let sock = home.join("luvus-client.sock");
        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            if server.child.try_wait().unwrap().is_some() {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(100));
        }
        if !sock.exists() {
            let mut diagnostics = String::new();
            std::io::Read::take(std::fs::File::open(home.join("startup.log")).unwrap(), 8192)
                .read_to_string(&mut diagnostics)
                .unwrap();
            panic!("server never created its client socket: {diagnostics}");
        }

        let conn = crate::ipc::transport::connect(&sock).unwrap();
        assert_eq!(
            conn.set_timeouts(std::time::Duration::from_secs(5))
                .unwrap(),
            crate::ipc::transport::TimeoutMode::Kernel,
            "Unix lifecycle tests require bounded blocking reads and writes"
        );
        let mut writer = conn.clone();
        let mut reader = std::io::BufReader::new(conn);

        // Drive the same handshake used by local and SSH-relayed clients.
        protocol::write_message(
            &mut writer,
            &ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                cols: 80,
                rows: 24,
            },
        )
        .unwrap();
        writer.flush().unwrap();

        // Welcome is backward-decodable, then the server explicitly requests
        // the palette from the terminal displaying this remote client.
        match protocol::read_message::<_, ServerMessage>(&mut reader).unwrap() {
            ServerMessage::Welcome { version, error } => {
                assert_eq!(version, PROTOCOL_VERSION);
                assert!(error.is_none(), "handshake error: {error:?}");
            }
            other => panic!(
                "expected Welcome, got a different message: {:?}",
                std::mem::discriminant(&other)
            ),
        }
        match protocol::read_message::<_, ServerMessage>(&mut reader).unwrap() {
            ServerMessage::Ready { probe_colors: true } => {}
            _ => panic!("terminal theme should request the client palette"),
        }
        let colors = crate::terminal::theme_probe::TerminalColors {
            fg: [238, 238, 238],
            bg: [20, 20, 20],
            palette: crate::terminal::theme_probe::default_ansi_palette(
                [238, 238, 238],
                [20, 20, 20],
            ),
        };
        protocol::write_message(
            &mut writer,
            &ClientMessage::TerminalProbe {
                colors: Some(colors),
                graphics: None,
            },
        )
        .unwrap();
        writer.flush().unwrap();

        let mut got_frame = false;
        for _ in 0..8 {
            match protocol::read_message::<_, ServerMessage>(&mut reader) {
                Ok(ServerMessage::Frame(fr)) => {
                    assert!(fr.width > 0 && fr.height > 0, "frame has real dimensions");
                    got_frame = true;
                    break;
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        assert!(
            got_frame,
            "the server returned a real frame after palette negotiation"
        );

        // `server` kills only the child handle spawned above, even if an
        // assertion panics. It never addresses an inherited production socket.
    }

    /// Subprocess entry point for `real_server_accepts_a_terminal_palette`.
    /// The ordinary test-suite invocation returns immediately; only the
    /// explicitly marked child process enters the blocking server loop.
    #[test]
    fn terminal_palette_server_helper() {
        if std::env::var_os("LUVUS_TEST_PALETTE_SERVER").is_some() {
            crate::ipc::server::run().expect("scratch server failed");
        }
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use ratatui::backend::TestBackend;

    #[test]
    fn paste_event_preserves_windows_paths_quotes_and_unicode() {
        let command = r#".\.venv\Scripts\python.exe .\youtube_folder_uploader.py --folder "E:\Vídeos\Pendientes €""#;
        let message = event_message(Event::Paste(command.to_string()));
        assert!(matches!(
            message,
            Some(ClientMessage::Paste(text)) if text == command
        ));
    }

    #[test]
    fn image_paste_chord_uses_binary_data_and_falls_back_to_the_key() {
        let key = ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Char('v'),
            ratatui::crossterm::event::KeyModifiers::CONTROL,
        );
        assert!(matches!(
            event_message_with_image(Event::Key(key), || Some(vec![1, 2, 3])),
            Some(ClientMessage::ClipboardImage(bytes)) if bytes == [1, 2, 3]
        ));
        assert!(matches!(
            event_message_with_image(Event::Key(key), || None),
            Some(ClientMessage::Key(fallback)) if fallback == key
        ));
    }

    #[test]
    fn session_handoff_replaces_only_the_session_selector() {
        let raw = vec![
            "luvus".to_string(),
            "--session".to_string(),
            "old".to_string(),
            "--remote".to_string(),
            "host".to_string(),
            "-p".to_string(),
            "2222".to_string(),
        ];
        assert_eq!(
            switched_args(&raw, "new"),
            ["--session", "new", "--remote", "host", "-p", "2222"]
        );
    }

    #[test]
    fn session_handoff_preserves_subcommand_session_flags() {
        let raw = vec![
            "luvus".to_string(),
            "--session".to_string(),
            "old".to_string(),
            "pane".to_string(),
            "report".to_string(),
            "--session".to_string(),
            "native-rollout".to_string(),
        ];
        assert_eq!(
            switched_args(&raw, "new"),
            [
                "--session",
                "new",
                "pane",
                "report",
                "--session",
                "native-rollout"
            ]
        );
    }

    /// Two users can watch the same pane through different terminals. The
    /// frame is one and the same, so the cell that carries an image has to be
    /// resolved per client: drawn where the terminal has the image, blanked
    /// where it does not — printing the private-use character instead would put
    /// a tofu box on that user's screen.
    #[test]
    fn an_image_cell_is_drawn_or_blanked_according_to_each_clients_terminal() {
        let image = "\u{10eeee}\u{0305}\u{0305}";
        // The id rides in the foreground; 42 is `Rgb(0, 0, 42)` on the wire.
        let fg = protocol::pack(ratatui::style::Color::Rgb(0, 0, 42));

        let drawing = make_cell(image, fg, 0, 0, HostTerminal::drawing());
        assert_eq!(drawing.symbol(), image, "the cell must reach the terminal");
        assert_eq!(
            drawing.fg,
            ratatui::style::Color::Rgb(0, 0, 42),
            "the image id must survive exactly, not be downsampled to a color"
        );

        // The same cell, at a terminal that cannot draw it.
        let text_only = make_cell(
            image,
            fg,
            0,
            0,
            HostTerminal {
                truecolor: true,
                graphics: false,
            },
        );
        assert_eq!(text_only.symbol(), " ", "no tofu on a terminal without it");

        // And at a terminal that draws images but downsamples color: the id
        // must still not be rewritten.
        let downsampling = make_cell(
            image,
            fg,
            0,
            0,
            HostTerminal {
                truecolor: false,
                graphics: true,
            },
        );
        assert_eq!(
            downsampling.fg,
            ratatui::style::Color::Rgb(0, 0, 42),
            "an image id is not a color and must escape the 256-color path"
        );

        // Ordinary text is unaffected by any of this.
        let text = make_cell(
            "x",
            protocol::pack(ratatui::style::Color::Rgb(200, 100, 50)),
            0,
            0,
            HostTerminal {
                truecolor: false,
                graphics: true,
            },
        );
        assert_eq!(text.symbol(), "x");
        assert!(
            !matches!(text.fg, ratatui::style::Color::Rgb(..)),
            "text still downsamples where the terminal needs it: {:?}",
            text.fg
        );
    }

    #[test]
    fn incremental_diff_reconstructs_the_screen() {
        let cell = |s: &str| protocol::CellData {
            symbol: s.into(),
            fg: 0,
            bg: 0,
            mods: 0,
        };
        let f0 = FrameData {
            width: 3,
            height: 1,
            cells: vec![cell("a"), cell("b"), cell("c")],
            cursor: None,
            cursor_visible: false,
        };
        let f1 = FrameData {
            width: 3,
            height: 1,
            cells: vec![cell("a"), cell("X"), cell("c")],
            cursor: Some((1, 0)),
            cursor_visible: true,
        };

        let mut term = Terminal::new(TestBackend::new(3, 1)).unwrap();
        let mut last_cursor = None;
        // Paint a full frame, then apply a diff that changes only one cell.
        paint(
            &mut term,
            &frame_cells(&f0, HostTerminal::drawing()),
            f0.cursor,
            f0.cursor_visible,
            true,
            &mut last_cursor,
        )
        .unwrap();
        let diff = FrameDiff {
            width: 3,
            height: 1,
            runs: protocol::diff_runs(&f0, &f1),
            cursor: f1.cursor,
            cursor_visible: f1.cursor_visible,
        };
        paint(
            &mut term,
            &diff_cells(&diff, HostTerminal::drawing()),
            diff.cursor,
            diff.cursor_visible,
            false,
            &mut last_cursor,
        )
        .unwrap();

        // The terminal now shows f1 — the client stays correct without ever
        // re-blitting the whole frame.
        let got = protocol::frame_from_buffer(term.backend().buffer(), None, false);
        assert_eq!(got.cells, f1.cells);
    }
}

#[cfg(test)]
mod paint_tests {
    use super::paint;
    use crate::ipc::protocol::{self, FrameData, FrameDiff};
    use ratatui::backend::{Backend, TestBackend};
    use ratatui::layout::Position;
    use ratatui::Terminal;

    fn cell(s: &str) -> protocol::CellData {
        protocol::CellData {
            symbol: s.into(),
            fg: 0,
            bg: 0,
            mods: 0,
        }
    }

    #[test]
    fn pty_visible_cursor_is_restored_after_spinner_like_diff() {
        let mut term = Terminal::new(TestBackend::new(8, 8)).unwrap();
        let mut cells = vec![cell(" "); 64];
        let f0 = FrameData {
            width: 8,
            height: 8,
            cells: cells.clone(),
            cursor: Some((1, 4)),
            cursor_visible: true,
        };
        let mut last = None;
        paint(
            &mut term,
            &super::frame_cells(&f0, super::HostTerminal::drawing()),
            f0.cursor,
            f0.cursor_visible,
            true,
            &mut last,
        )
        .unwrap();

        // Crossterm's draw walks the hardware cursor onto each cell. TestBackend
        // does not, so park it on the bottom-right spinner cell the same way.
        term.backend_mut()
            .set_cursor_position(Position::new(7, 7))
            .unwrap();

        cells[63] = cell("*");
        let f1 = FrameData {
            width: 8,
            height: 8,
            cells,
            cursor: Some((1, 4)),
            cursor_visible: true,
        };
        let diff = FrameDiff {
            width: 8,
            height: 8,
            runs: protocol::diff_runs(&f0, &f1),
            cursor: Some((1, 4)),
            cursor_visible: true,
        };
        paint(
            &mut term,
            &super::diff_cells(&diff, super::HostTerminal::drawing()),
            diff.cursor,
            diff.cursor_visible,
            false,
            &mut last,
        )
        .unwrap();

        assert_eq!(
            term.backend_mut().get_cursor_position().unwrap(),
            Position::new(1, 4)
        );
        assert!(term.backend().cursor_visible());
    }

    #[test]
    fn hidden_pty_cursor_does_not_show_a_luvus_caret() {
        let mut term = Terminal::new(TestBackend::new(8, 8)).unwrap();
        let mut cells = vec![cell(" "); 64];
        let f0 = FrameData {
            width: 8,
            height: 8,
            cells: cells.clone(),
            cursor: Some((1, 4)),
            cursor_visible: true,
        };
        let mut last = None;
        paint(
            &mut term,
            &super::frame_cells(&f0, super::HostTerminal::drawing()),
            f0.cursor,
            f0.cursor_visible,
            true,
            &mut last,
        )
        .unwrap();
        assert!(term.backend().cursor_visible());

        term.backend_mut()
            .set_cursor_position(Position::new(7, 7))
            .unwrap();

        cells[63] = cell("*");
        let f1 = FrameData {
            width: 8,
            height: 8,
            cells,
            cursor: Some((1, 4)),
            cursor_visible: false,
        };
        let diff = FrameDiff {
            width: 8,
            height: 8,
            runs: protocol::diff_runs(&f0, &f1),
            cursor: Some((1, 4)),
            cursor_visible: false,
        };
        paint(
            &mut term,
            &super::diff_cells(&diff, super::HostTerminal::drawing()),
            diff.cursor,
            diff.cursor_visible,
            false,
            &mut last,
        )
        .unwrap();

        assert_eq!(
            term.backend_mut().get_cursor_position().unwrap(),
            Position::new(1, 4)
        );
        assert!(!term.backend().cursor_visible());
    }

    #[test]
    fn out_of_bounds_cursor_hides_caret() {
        let mut term = Terminal::new(TestBackend::new(8, 8)).unwrap();
        let mut last = None;
        let cells = vec![cell(" "); 64];
        let f0 = FrameData {
            width: 8,
            height: 8,
            cells,
            cursor: Some((1, 4)),
            cursor_visible: true,
        };
        paint(
            &mut term,
            &super::frame_cells(&f0, super::HostTerminal::drawing()),
            f0.cursor,
            f0.cursor_visible,
            true,
            &mut last,
        )
        .unwrap();

        paint(&mut term, &[], Some((99, 99)), false, false, &mut last).unwrap();

        assert_eq!(
            term.backend_mut().get_cursor_position().unwrap(),
            Position::new(1, 4)
        );
        assert!(!term.backend().cursor_visible());
    }

    #[test]
    fn hidden_in_view_pty_parks_without_showing() {
        let mut term = Terminal::new(TestBackend::new(8, 8)).unwrap();
        let mut last = None;
        let cells = vec![cell(" "); 64];
        let f0 = FrameData {
            width: 8,
            height: 8,
            cells,
            cursor: Some((3, 5)),
            cursor_visible: false,
        };
        paint(
            &mut term,
            &super::frame_cells(&f0, super::HostTerminal::drawing()),
            f0.cursor,
            f0.cursor_visible,
            true,
            &mut last,
        )
        .unwrap();

        assert_eq!(
            term.backend_mut().get_cursor_position().unwrap(),
            Position::new(3, 5)
        );
        assert!(!term.backend().cursor_visible());
    }

    #[test]
    fn tab_row_caret_stays_put_on_a_tall_screen() {
        assert_eq!(super::ime_position(Some((3, 0)), 80, 24), Some((3, 0)));
        assert_eq!(super::ime_position(Some((3, 1)), 80, 24), Some((3, 1)));
    }

    #[test]
    fn grok_prompt_caret_stays_put() {
        assert_eq!(super::ime_position(Some((4, 20)), 80, 24), Some((4, 20)));
    }
}
