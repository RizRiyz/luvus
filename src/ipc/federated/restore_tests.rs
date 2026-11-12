//! Resize/recovery races use actual client routing, wire requests, and paint output.
use super::graphics_tests::Output;
use super::*;
use ratatui::layout::Size;

fn frame(cols: u16, rows: u16) -> protocol::FrameData {
    let mut f = protocol::frame_from_buffer(
        &ratatui::buffer::Buffer::empty(Rect::new(0, 0, cols, rows)),
        Some((3, 4)),
        true,
        protocol::HostCursorStyle::default(),
    );
    f.graphics = b"\x1b_Ga=t,i=10000;AAAA\x1b\\\x1b_Ga=p,i=10000,p=1;\x1b\\".to_vec();
    f
}

#[test]
fn restore_resize_replaces_enqueued_ticket_and_timeout_restores_only_active_input() {
    let _env = crate::persist::test_env("modal-resize-replay");
    let path = crate::persist::ensure_config_dir().join("s");
    let listener = crate::ipc::transport::bind(&path).unwrap();
    let local = crate::ipc::transport::connect(&path).unwrap();
    let mut peer = crate::ipc::transport::incoming(&listener).next().unwrap();
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
    open_selector(&mut dock, &active, &machines);
    paint_dock(
        &mut terminal,
        &mut dock,
        &machines,
        &active,
        &mut cursor,
        visible,
    )
    .unwrap();
    close_selector(&mut dock, &active, &mut candidate, &machines, &writer).unwrap();
    let ticket = candidate.as_ref().unwrap().ticket;
    let deadline = candidate.as_ref().unwrap().deadline;
    assert!(
        matches!(protocol::read_message::<_, ClientMessage>(&mut peer).unwrap(),ClientMessage::PrepareSurface{ticket:t,cols:80,rows:24} if t==ticket)
    );
    // The first full frame is already enqueued, but physical size changed before
    // the input resize event was dequeued. Reject AND request a new ticket.
    terminal.backend_mut().size = Some(Size::new(70, 22));
    let before = terminal.backend_mut().bytes.clone();
    handle_surface_message(
        Endpoint::Local,
        ServerMessage::PreparedFrame {
            ticket,
            frame: frame(80, 24),
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
    let refreshed = candidate.as_ref().unwrap().ticket;
    assert_ne!(refreshed, ticket);
    assert!(
        matches!(protocol::read_message::<_, ClientMessage>(&mut peer).unwrap(),ClientMessage::PrepareSurface{ticket:t,cols:70,rows:22} if t==refreshed)
    );
    let (events, _rx) = mpsc::sync_channel(8);
    let mut pending = None;
    // The queued Resize now matches the viewport already re-ticketed by the
    // old PreparedFrame, but still carries a new font/scale pixel pair.
    for pixels in [(12, 24), (15, 30)] {
        assert!(handle_dock_input(
            &ClientMessage::Resize {
                cols: 70,
                rows: 22,
                cell_width_px: pixels.0,
                cell_height_px: pixels.1
            },
            &mut dock,
            &active,
            &mut candidate,
            &mut pending,
            &mut machines,
            &writer,
            &mut terminal,
            &events
        )
        .unwrap());
        assert!(
            matches!(protocol::read_message::<_,ClientMessage>(&mut peer).unwrap(), ClientMessage::CellPixels { cell_width_px,cell_height_px } if (cell_width_px,cell_height_px)==pixels)
        );
        assert_eq!(candidate.as_ref().unwrap().ticket, refreshed);
        assert_eq!(candidate.as_ref().unwrap().deadline, deadline);
        assert_eq!(
            terminal.backend_mut().bytes,
            before,
            "pixel metadata cannot paint old frames"
        );
    }
    // The very next wire command is the following resize's CellPixels (below),
    // proving the two same-size updates emitted no extra ticket or Resize.
    // Subsequent resize events each consume routing and issue exactly one new
    // PrepareSurface (not an ordinary Resize after a ticket).
    let mut old = vec![(ticket, 80, 24), (refreshed, 70, 22)];
    for (cols, rows) in [(60, 20), (50, 18)] {
        terminal.backend_mut().size = Some(Size::new(cols, rows));
        assert!(handle_dock_input(
            &ClientMessage::Resize {
                cols,
                rows,
                cell_width_px: 10,
                cell_height_px: 10
            },
            &mut dock,
            &active,
            &mut candidate,
            &mut pending,
            &mut machines,
            &writer,
            &mut terminal,
            &events
        )
        .unwrap());
        let p = candidate.as_ref().unwrap();
        assert_eq!(p.deadline, deadline);
        assert!(matches!(
            protocol::read_message::<_, ClientMessage>(&mut peer).unwrap(),
            ClientMessage::CellPixels {
                cell_width_px: 10,
                cell_height_px: 10
            }
        ));
        assert!(
            matches!(protocol::read_message::<_, ClientMessage>(&mut peer).unwrap(),ClientMessage::PrepareSurface{ticket:t,cols:c,rows:r} if t==p.ticket && c==cols && r==rows)
        );
        old.push((p.ticket, cols, rows));
    }
    let current = candidate.as_ref().unwrap().ticket;
    for (old_ticket, cols, rows) in old.into_iter().filter(|(t, _, _)| *t != current) {
        handle_surface_message(
            Endpoint::Local,
            ServerMessage::PreparedFrame {
                ticket: old_ticket,
                frame: frame(cols, rows),
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
    }
    assert_eq!(terminal.backend_mut().bytes, before);
    // A forged boot on an otherwise exact ticket never paints.
    candidate.as_mut().unwrap().boot_id = Some(9);
    handle_surface_message(
        Endpoint::Local,
        ServerMessage::PreparedFrame {
            ticket: current,
            frame: frame(50, 18),
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
    candidate.as_mut().unwrap().boot_id = None;
    handle_surface_message(
        Endpoint::Local,
        ServerMessage::PreparedFrame {
            ticket: current,
            frame: frame(50, 18),
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
    assert!(candidate.is_none());
    assert!(!dock.restore_blocked);
    assert_eq!(active, Endpoint::Local);
    assert_eq!(cursor, Some((3, 4)));
    assert!(matches!(
        protocol::read_message::<_, ClientMessage>(&mut peer).unwrap(),
        ClientMessage::SurfaceInterest(SurfaceInterest::Active)
    ));
    // Same-endpoint restore timeout reclaims Active, not Suspended; queued
    // ordinary frames and old tickets remain visually fenced afterward.
    request_modal_restore(
        &mut dock,
        &active,
        &mut candidate,
        &machines,
        &writer,
        (50, 18),
    )
    .unwrap();
    let failed = candidate.as_ref().unwrap().ticket;
    assert!(matches!(
        protocol::read_message::<_, ClientMessage>(&mut peer).unwrap(),
        ClientMessage::PrepareSurface { .. }
    ));
    candidate.as_mut().unwrap().deadline = Instant::now();
    assert!(expire_candidate(&mut candidate, &active, &mut dock, &machines, &writer).unwrap());
    assert!(matches!(
        protocol::read_message::<_, ClientMessage>(&mut peer).unwrap(),
        ClientMessage::SurfaceInterest(SurfaceInterest::Active)
    ));
    assert!(candidate.is_none());
    assert!(dock.restore_blocked);
    let before = terminal.backend_mut().bytes.clone();
    for message in [
        ServerMessage::Frame(frame(50, 18)),
        ServerMessage::PreparedFrame {
            ticket: failed,
            frame: frame(50, 18),
        },
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
    // An explicit resize at the same size retries a failed restore, while an
    // already-pending duplicate is consumed without invalidating its ticket.
    let resize = ClientMessage::Resize {
        cols: 50,
        rows: 18,
        cell_width_px: 10,
        cell_height_px: 10,
    };
    assert!(handle_dock_input(
        &resize,
        &mut dock,
        &active,
        &mut candidate,
        &mut pending,
        &mut machines,
        &writer,
        &mut terminal,
        &events
    )
    .unwrap());
    assert!(matches!(
        protocol::read_message::<_, ClientMessage>(&mut peer).unwrap(),
        ClientMessage::CellPixels { .. }
    ));
    let retry = candidate.as_ref().unwrap().ticket;
    assert!(
        matches!(protocol::read_message::<_,ClientMessage>(&mut peer).unwrap(),ClientMessage::PrepareSurface{ticket,..} if ticket==retry)
    );
    assert!(handle_dock_input(
        &resize,
        &mut dock,
        &active,
        &mut candidate,
        &mut pending,
        &mut machines,
        &writer,
        &mut terminal,
        &events
    )
    .unwrap());
    assert_eq!(candidate.as_ref().unwrap().ticket, retry);
    assert!(matches!(
        protocol::read_message::<_, ClientMessage>(&mut peer).unwrap(),
        ClientMessage::CellPixels {
            cell_width_px: 10,
            cell_height_px: 10
        }
    ));
    handle_surface_message(
        Endpoint::Local,
        ServerMessage::PreparedFrame {
            ticket: retry,
            frame: frame(50, 18),
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
    assert!(!dock.restore_blocked);
    assert!(candidate.is_none());
    assert!(matches!(
        protocol::read_message::<_, ClientMessage>(&mut peer).unwrap(),
        ClientMessage::SurfaceInterest(SurfaceInterest::Active)
    ));
    // A normal switch toward Local times out while Remote remains active:
    // only the failed Local candidate is suspended, never activated or painted.
    let active_remote = Endpoint::Remote {
        machine_id: "current".into(),
        session: "review".into(),
    };
    candidate = Some(SurfaceCandidate {
        ticket: 999,
        endpoint: Endpoint::Local,
        welcomed: true,
        ready: true,
        shell_dock: None,
        boot_id: None,
        deadline: Instant::now(),
    });
    assert!(expire_candidate(
        &mut candidate,
        &active_remote,
        &mut dock,
        &machines,
        &writer
    )
    .unwrap());
    assert!(matches!(
        protocol::read_message::<_, ClientMessage>(&mut peer).unwrap(),
        ClientMessage::SurfaceInterest(SurfaceInterest::Suspended)
    ));
    assert!(candidate.is_none());
    assert_eq!(
        active_remote,
        Endpoint::Remote {
            machine_id: "current".into(),
            session: "review".into()
        }
    );
}

#[test]
fn preparation_failure_rolls_back_input_without_activating_switch_destination() {
    let mut dock = DockState::default();
    let active = Endpoint::Local;
    let make = |endpoint| {
        Some(SurfaceCandidate {
            ticket: 1,
            endpoint,
            welcomed: true,
            ready: true,
            shell_dock: None,
            boot_id: None,
            deadline: Instant::now(),
        })
    };
    let mut candidate = make(active.clone());
    let mut sent = Vec::new();
    cancel_preparation(&mut candidate, &active, &mut dock, |e, i| {
        sent.push((e.clone(), i));
        Ok(())
    })
    .unwrap();
    assert_eq!(sent, vec![(active.clone(), SurfaceInterest::Active)]);
    assert!(dock.restore_blocked);
    let remote = Endpoint::Remote {
        machine_id: "requested".into(),
        session: "review".into(),
    };
    dock.restore_blocked = false;
    candidate = make(remote.clone());
    sent.clear();
    cancel_preparation(&mut candidate, &active, &mut dock, |e, i| {
        sent.push((e.clone(), i));
        Ok(())
    })
    .unwrap();
    assert_eq!(sent, vec![(remote, SurfaceInterest::Suspended)]);
    assert!(!dock.restore_blocked);
    assert_eq!(active, Endpoint::Local);
    candidate = make(active.clone());
    assert!(
        cancel_preparation(&mut candidate, &active, &mut dock, |_, _| Err(anyhow!(
            "closed control"
        )))
        .is_err()
    );
    assert!(candidate.is_none());
    assert!(dock.restore_blocked);
    // A transient control-queue failure may reject PrepareSurface but accept
    // Active rollback; keep input and the visual fence, with no destination switch.
    let mut calls = Vec::new();
    assert!(modal_restore_with_send(
        &mut dock,
        &active,
        &mut candidate,
        None,
        (80, 24),
        |message| {
            match message {
                ClientMessage::PrepareSurface { .. } => {
                    calls.push("prepare");
                    Err(anyhow!("queue full"))
                }
                ClientMessage::SurfaceInterest(SurfaceInterest::Active) => {
                    calls.push("active");
                    Ok(())
                }
                _ => panic!("unexpected recovery command"),
            }
        }
    )
    .is_ok());
    assert_eq!(calls, vec!["prepare", "active"]);
    assert!(candidate.is_none());
    candidate = make(active.clone());
    calls.clear();
    assert!(
        !forward_restore_pixels(&mut dock, &active, &mut candidate, (12, 24), |message| {
            match message {
                ClientMessage::CellPixels {
                    cell_width_px: 12,
                    cell_height_px: 24,
                } => {
                    calls.push("pixels");
                    Err(anyhow!("queue full"))
                }
                ClientMessage::SurfaceInterest(SurfaceInterest::Active) => {
                    calls.push("active");
                    Ok(())
                }
                _ => panic!("pixel failure must not prepare or resize"),
            }
        })
        .unwrap()
    );
    assert_eq!(calls, vec!["pixels", "active"]);
    assert!(candidate.is_none());
    assert!(dock.restore_blocked);
    candidate = make(active.clone());
    assert!(
        forward_restore_pixels(&mut dock, &active, &mut candidate, (12, 24), |_| Err(
            anyhow!("closed transport")
        ))
        .is_err()
    );
    assert!(candidate.is_none());
    assert!(dock.restore_blocked);
    assert!(
        modal_restore_with_send(&mut dock, &active, &mut candidate, None, (80, 24), |_| Err(
            anyhow!("closed transport")
        ))
        .is_err()
    );
    assert!(candidate.is_none());
    assert!(dock.restore_blocked);
}
