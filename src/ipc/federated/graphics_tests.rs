//! Real federated paint output on an in-memory backend and isolated control socket.
use super::test_support::ClientHarness;
use super::*;
use std::io::{self, Write};

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
    let mut h = ClientHarness::new();
    let mut frame = protocol::frame_from_buffer(
        &ratatui::buffer::Buffer::empty(Rect::new(0, 0, 80, 24)),
        Some((3, 4)),
        true,
        protocol::HostCursorStyle::default(),
    );
    frame.graphics =
        b"\x1b_Ga=t,i=10000;AAAA\x1b\\\x1b_Ga=p,i=10000,p=1,c=10,r=10,z=1;\x1b\\".to_vec();
    for popup in 0..3 {
        h.surface(Endpoint::Local, ServerMessage::Frame(frame.clone()))
            .unwrap();
        h.terminal.backend_mut().bytes.clear();
        match popup {
            0 => h.open_selector(),
            1 => {
                h.dock.machine_popup = Some(MachinePopup::Menu {
                    machine_id: "other".into(),
                    label: "other".into(),
                    anchor: (20, 10),
                    rect: None,
                    remove_rect: None,
                    selected: 0,
                })
            }
            _ => {
                h.dock.machine_popup = Some(MachinePopup::Confirm {
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
        h.paint_dock().unwrap();
        let bytes = String::from_utf8_lossy(&h.terminal.backend_mut().bytes).into_owned();
        let deleted = bytes.find("a=d,d=i,i=10000,q=2").unwrap();
        assert!(
            deleted < bytes.find("<hide>").unwrap(),
            "delete before painting popup"
        );
        assert!(!bytes.contains("d=A"));
        assert!(!bytes.contains("d=I"));
        let before = h.terminal.backend_mut().bytes.clone();
        let remote = Endpoint::Remote {
            machine_id: "inactive".into(),
            session: "default".into(),
        };
        h.surface(remote, ServerMessage::Frame(frame.clone()))
            .unwrap();
        assert_eq!(
            h.terminal.backend_mut().bytes,
            before,
            "inactive endpoint never paints"
        );
        h.close_selector().unwrap();
        let ticket = h.candidate.as_ref().unwrap().ticket;
        let before = h.terminal.backend_mut().bytes.clone();
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
            h.surface(Endpoint::Local, message).unwrap();
        }
        assert_eq!(h.terminal.backend_mut().bytes, before);
        if popup == 0 {
            // Reopening while a replay is in flight cannot let its old ticket
            // paint through the modal or leave the next close permanently stuck.
            h.open_selector();
            h.surface(
                Endpoint::Local,
                ServerMessage::PreparedFrame {
                    ticket,
                    frame: frame.clone(),
                },
            )
            .unwrap();
            assert_eq!(h.terminal.backend_mut().bytes, before);
            h.close_selector().unwrap();
            assert_ne!(h.candidate.as_ref().unwrap().ticket, ticket);
        }
        let ticket = h.candidate.as_ref().unwrap().ticket;
        h.surface(
            Endpoint::Local,
            ServerMessage::PreparedFrame {
                ticket,
                frame: frame.clone(),
            },
        )
        .unwrap();
        assert_eq!(h.active, Endpoint::Local);
        assert!(h.candidate.is_none());
        assert_eq!(h.cursor, Some((3, 4)));
        assert!(h.visible);
        let bytes = String::from_utf8_lossy(&h.terminal.backend_mut().bytes);
        let clear = bytes.rfind("<clear>").unwrap();
        let upload = bytes.rfind("a=t,i=10000").unwrap();
        let place = bytes.rfind("a=p,i=10000").unwrap();
        assert!(clear < upload && upload < place);
    }
    // Cancelling a popup during a user-requested switch must preserve that
    // exact remote h.candidate, not queue a restore of the old Local endpoint.
    h.candidate = Some(SurfaceCandidate {
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
    h.close_selector().unwrap();
    assert_eq!(h.candidate.as_ref().unwrap().ticket, 123);
    assert_eq!(h.active, Endpoint::Local);
}
