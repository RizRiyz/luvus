//! Real federated paint output on an in-memory backend and isolated control socket.
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

#[test]
fn host_placement_inventory_deletes_only_displayed_ids_and_retries_io_failure() {
    let mut inventory = graphics::HostPlacements::default();
    inventory.observe(
        b"\x1b_Ga=t,i=10001;AAAA\x1b\\\x1b_Ga=p,i=10000,p=1;\x1b\\\x1b_Ga=p,i=1,p=1;\x1b\\",
    );
    inventory.observe(b"\x1b_Ga=d,d=i,i=10000,p=1;\x1b\\");
    struct Fail;
    impl Write for Fail {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::WouldBlock.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    assert!(inventory.hide(&mut Fail).is_err());
    let mut bytes = Vec::new();
    inventory.hide(&mut bytes).unwrap();
    assert_eq!(bytes, b"\x1b_Ga=d,d=i,i=10000,q=2;\x1b\\");
    bytes.clear();
    inventory.hide(&mut bytes).unwrap();
    assert!(bytes.is_empty());
    inventory.observe(b"\x1b_Ga=p,i=10002,p=1;\x1b\\\x1b_Ga=d,d=I,i=10002;\x1b\\");
    inventory.hide(&mut bytes).unwrap();
    assert!(bytes.is_empty());
}

#[test]
fn federated_selector_and_delete_hide_owned_placements_then_ticketed_replay() {
    let _env = crate::persist::test_env("federated-graphics-output");
    let path = crate::persist::ensure_config_dir().join("s");
    let listener = crate::ipc::transport::bind(&path).unwrap();
    let local = crate::ipc::transport::connect(&path).unwrap();
    let _peer = crate::ipc::transport::incoming(&listener).next().unwrap();
    let writer = Arc::new(Mutex::new(local));
    let mut terminal = ratatui::Terminal::with_options(
        Output::default(),
        ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Fixed(Rect::new(0, 0, 80, 24)),
        },
    )
    .unwrap();
    let mut dock = DockState::default();
    let mut active = Endpoint::Local;
    let mut candidate = None;
    let mut machines = HashMap::new();
    let mut cursor = None;
    let mut style = super::super::client::AppliedHostCursor::default();
    let mut visible = false;
    let mut frame = protocol::frame_from_buffer(
        &ratatui::buffer::Buffer::empty(Rect::new(0, 0, 80, 24)),
        Some((3, 4)),
        true,
        protocol::HostCursorStyle::default(),
    );
    frame.graphics =
        b"\x1b_Ga=t,i=10000;AAAA\x1b\\\x1b_Ga=p,i=10000,p=1,c=10,r=10,z=1;\x1b\\".to_vec();
    for popup in 0..3 {
        handle_surface_message(
            Endpoint::Local,
            ServerMessage::Frame(frame.clone()),
            &mut active,
            &mut candidate,
            &mut machines,
            &writer,
            &mut terminal,
            true,
            true,
            &mut dock,
            &mut cursor,
            &mut style,
            &mut visible,
        )
        .unwrap();
        terminal.backend_mut().bytes.clear();
        match popup {
            0 => open_selector(&mut dock, &active, &machines),
            1 => {
                dock.machine_popup = Some(MachinePopup::Menu {
                    machine_id: "other".into(),
                    label: "other".into(),
                    anchor: (20, 10),
                    rect: None,
                    remove_rect: None,
                    selected: 0,
                })
            }
            _ => {
                dock.machine_popup = Some(MachinePopup::Confirm {
                    machine_id: "other".into(),
                    label: "other".into(),
                    rect: None,
                    confirm_rect: None,
                    cancel_rect: None,
                    removing: false,
                    error: None,
                })
            }
        }
        // Entry points use the real shell renderer, not synthetic delete bytes.
        paint_dock(
            &mut terminal,
            &mut dock,
            &machines,
            &active,
            &mut cursor,
            visible,
        )
        .unwrap();
        let bytes = String::from_utf8_lossy(&terminal.backend_mut().bytes).into_owned();
        let deleted = bytes.find("a=d,d=i,i=10000,q=2").unwrap();
        assert!(
            deleted < bytes.find("<hide>").unwrap(),
            "delete before painting popup"
        );
        assert!(!bytes.contains("d=A"));
        assert!(!bytes.contains("d=I"));
        let before = terminal.backend_mut().bytes.clone();
        let remote = Endpoint::Remote {
            machine_id: "inactive".into(),
            session: "default".into(),
        };
        handle_surface_message(
            remote,
            ServerMessage::Frame(frame.clone()),
            &mut active,
            &mut candidate,
            &mut machines,
            &writer,
            &mut terminal,
            true,
            true,
            &mut dock,
            &mut cursor,
            &mut style,
            &mut visible,
        )
        .unwrap();
        assert_eq!(
            terminal.backend_mut().bytes,
            before,
            "inactive endpoint never paints"
        );
        close_selector(&mut dock, &active, &mut candidate, &machines, &writer).unwrap();
        let ticket = candidate.as_ref().unwrap().ticket;
        let before = terminal.backend_mut().bytes.clone();
        let mut wrong_size = frame.clone();
        wrong_size.width = 79;
        // Old full frames, diffs and ticket mismatches cannot undo the modal.
        for message in [
            ServerMessage::Frame(frame.clone()),
            ServerMessage::PreparedFrame {
                ticket: ticket.wrapping_sub(1),
                frame: frame.clone(),
            },
            ServerMessage::PreparedFrame {
                ticket,
                frame: wrong_size,
            },
            ServerMessage::FrameDiff(protocol::FrameDiff {
                width: 80,
                height: 24,
                runs: Vec::new(),
                cursor: Some((1, 1)),
                cursor_visible: true,
                cursor_style: protocol::HostCursorStyle::default(),
                graphics: frame.graphics.clone(),
            }),
        ] {
            handle_surface_message(
                Endpoint::Local,
                message,
                &mut active,
                &mut candidate,
                &mut machines,
                &writer,
                &mut terminal,
                true,
                true,
                &mut dock,
                &mut cursor,
                &mut style,
                &mut visible,
            )
            .unwrap();
        }
        assert_eq!(terminal.backend_mut().bytes, before);
        if popup == 0 {
            // Reopening while a replay is in flight cannot let its old ticket
            // paint through the modal or leave the next close permanently stuck.
            open_selector(&mut dock, &active, &machines);
            handle_surface_message(
                Endpoint::Local,
                ServerMessage::PreparedFrame {
                    ticket,
                    frame: frame.clone(),
                },
                &mut active,
                &mut candidate,
                &mut machines,
                &writer,
                &mut terminal,
                true,
                true,
                &mut dock,
                &mut cursor,
                &mut style,
                &mut visible,
            )
            .unwrap();
            assert_eq!(terminal.backend_mut().bytes, before);
            close_selector(&mut dock, &active, &mut candidate, &machines, &writer).unwrap();
            assert_ne!(candidate.as_ref().unwrap().ticket, ticket);
        }
        let ticket = candidate.as_ref().unwrap().ticket;
        handle_surface_message(
            Endpoint::Local,
            ServerMessage::PreparedFrame {
                ticket,
                frame: frame.clone(),
            },
            &mut active,
            &mut candidate,
            &mut machines,
            &writer,
            &mut terminal,
            true,
            true,
            &mut dock,
            &mut cursor,
            &mut style,
            &mut visible,
        )
        .unwrap();
        assert_eq!(active, Endpoint::Local);
        assert!(candidate.is_none());
        assert_eq!(cursor, Some((3, 4)));
        assert!(visible);
        let bytes = String::from_utf8_lossy(&terminal.backend_mut().bytes);
        let clear = bytes.rfind("<clear>").unwrap();
        let upload = bytes.rfind("a=t,i=10000").unwrap();
        let place = bytes.rfind("a=p,i=10000").unwrap();
        assert!(clear < upload && upload < place);
    }
    // Cancelling a popup during a user-requested switch must preserve that
    // exact remote candidate, not queue a restore of the old Local endpoint.
    candidate = Some(SurfaceCandidate {
        ticket: 123,
        endpoint: Endpoint::Remote {
            machine_id: "chosen".into(),
            session: "review".into(),
        },
        welcomed: true,
        ready: true,
        shell_dock: None,
        boot_id: Some(7),
        deadline: Instant::now() + SURFACE_PREPARE_TIMEOUT,
    });
    close_selector(&mut dock, &active, &mut candidate, &machines, &writer).unwrap();
    assert_eq!(candidate.as_ref().unwrap().ticket, 123);
    assert_eq!(active, Endpoint::Local);
}
