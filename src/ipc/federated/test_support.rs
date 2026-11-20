//! Small test-only client fixture; all routing and paint work uses production code.
use super::*;
use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::layout::{Position, Size};
use std::io::{self, Write};

#[derive(Default)]
pub(super) struct Output {
    pub(super) bytes: Vec<u8>,
    cursor: Position,
    pub(super) size: Option<Size>,
}
impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Backend for Output {
    type Error = io::Error;
    fn draw<'a, I>(&mut self, cells: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        for (x, y, c) in cells {
            write!(self, "\x1b[{};{}H{}", y + 1, x + 1, c.symbol())?;
        }
        Ok(())
    }
    fn hide_cursor(&mut self) -> io::Result<()> {
        self.write_all(b"<hide>")
    }
    fn show_cursor(&mut self) -> io::Result<()> {
        self.write_all(b"<show>")
    }
    fn get_cursor_position(&mut self) -> io::Result<Position> {
        Ok(self.cursor)
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, p: P) -> io::Result<()> {
        self.cursor = p.into();
        Ok(())
    }
    fn clear(&mut self) -> io::Result<()> {
        self.write_all(b"<clear>")
    }
    fn clear_region(&mut self, _: ClearType) -> io::Result<()> {
        Ok(())
    }
    fn size(&self) -> io::Result<Size> {
        Ok(self.size.unwrap_or(Size::new(80, 24)))
    }
    fn window_size(&mut self) -> io::Result<WindowSize> {
        Ok(WindowSize {
            columns_rows: Size::new(80, 24),
            pixels: Size::new(800, 240),
        })
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) struct ClientHarness {
    pub dock: DockState,
    pub active: Endpoint,
    pub candidate: Option<SurfaceCandidate>,
    pub machines: HashMap<String, MachineRuntime>,
    pub terminal: ratatui::Terminal<Output>,
    pub cursor: Option<(u16, u16)>,
    pub visible: bool,
    style: super::super::client::AppliedHostCursor,
    writer: Arc<Mutex<crate::ipc::transport::Conn>>,
    peer: crate::ipc::transport::Conn,
    _listener: crate::ipc::transport::Listener,
    events: Sender<ShellEvent>,
    _events_rx: mpsc::Receiver<ShellEvent>,
    pending: Option<Endpoint>,
}

impl ClientHarness {
    pub fn new() -> Self {
        let path = crate::persist::ensure_config_dir().join("s");
        let listener = crate::ipc::transport::bind(&path).unwrap();
        let local = crate::ipc::transport::connect(&path).unwrap();
        let peer = crate::ipc::transport::incoming(&listener).next().unwrap();
        let (events, rx) = mpsc::sync_channel(8);
        Self {
            dock: DockState::default(),
            active: Endpoint::Local,
            candidate: None,
            machines: HashMap::new(),
            terminal: ratatui::Terminal::with_options(
                Output::default(),
                ratatui::TerminalOptions {
                    viewport: ratatui::Viewport::Fixed(Rect::new(0, 0, 80, 24)),
                },
            )
            .unwrap(),
            cursor: None,
            visible: false,
            style: super::super::client::AppliedHostCursor::default(),
            writer: Arc::new(Mutex::new(local)),
            peer,
            _listener: listener,
            events,
            _events_rx: rx,
            pending: None,
        }
    }
    pub fn surface(&mut self, endpoint: Endpoint, message: ServerMessage) -> Result<()> {
        let exit = handle_surface_message(
            endpoint,
            message,
            &mut self.active,
            &mut self.candidate,
            &mut self.machines,
            &self.writer,
            &mut self.terminal,
            true,
            true,
            &mut self.dock,
            &mut self.cursor,
            &mut self.style,
            &mut self.visible,
        )?;
        assert!(exit.is_none(), "unexpected client exit");
        Ok(())
    }
    pub fn input(&mut self, message: &ClientMessage) -> Result<bool> {
        handle_dock_input(
            message,
            &mut self.dock,
            &self.active,
            &mut self.candidate,
            &mut self.pending,
            &mut self.machines,
            &self.writer,
            &mut self.terminal,
            &self.events,
        )
    }
    pub fn open_selector(&mut self) {
        open_selector(&mut self.dock, &self.active, &self.machines);
    }
    pub fn paint_dock(&mut self) -> Result<()> {
        paint_dock(
            &mut self.terminal,
            &mut self.dock,
            &self.machines,
            &self.active,
            &mut self.cursor,
            self.visible,
        )
    }
    pub fn close_selector(&mut self) -> Result<()> {
        close_selector(
            &mut self.dock,
            &self.active,
            &mut self.candidate,
            &self.machines,
            &self.writer,
        )
    }
    pub fn restore(&mut self, size: (u16, u16)) -> Result<()> {
        request_modal_restore(
            &mut self.dock,
            &self.active,
            &mut self.candidate,
            &self.machines,
            &self.writer,
            size,
        )
    }
    pub fn expire(&mut self) -> Result<bool> {
        self.expire_for(self.active.clone())
    }
    pub fn expire_for(&mut self, active: Endpoint) -> Result<bool> {
        expire_candidate(
            &mut self.candidate,
            &active,
            &mut self.dock,
            &self.machines,
            &self.writer,
        )
    }
    pub fn wire(&mut self) -> ClientMessage {
        protocol::read_message(&mut self.peer).unwrap()
    }
}
