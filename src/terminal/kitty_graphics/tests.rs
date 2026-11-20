//! Existing cross-boundary parser/pane/host characterization tests.
use super::*;

#[test]
fn occlusion_center_crops_offsets_and_restores_without_upload() {
    use ratatui::layout::Rect;
    let mut p = screen(1, visible(0, 0, 0));
    p.visible.grid_cols = 10;
    p.visible.grid_rows = 10;
    p.visible.image_width = 100;
    p.visible.image_height = 100;
    p.visible.x_offset = 5;
    p.visible.y_offset = 5;
    let mut occlusion = Occlusion::default();
    occlusion.cover(Rect::new(5, 4, 2, 2));
    let input = [p.clone()];
    let pieces = occlusion::frame_pieces(&input, &occlusion);
    // Compare source boundaries (95 displayed pixels after leading offset).
    assert_eq!(pieces.len(), 4);
    let top = pieces.iter().find(|(_, c, _)| c.y == 1).unwrap().1;
    assert_eq!((top.source_y, top.source_height, top.y_offset), (0, 26, 5));
    let right = pieces.iter().find(|(_, c, _)| c.x == 7).unwrap().1;
    assert_eq!(
        (right.source_x, right.source_width, right.x_offset),
        (47, 53, 0)
    );
    let screens = [p];
    let live = HashSet::from([1]);
    let retained = retained_of(&screens);
    let mut cache = HostGraphicsCache::default();
    let first =
        encode_occluded_host_frame(&mut cache, &screens, &live, &live, &retained, &occlusion);
    assert_eq!(text(&first).matches("a=t,").count(), 1);
    assert_eq!(cache.placements.len(), 4);
    let ids = cache.host_placements.clone();
    assert!(
        encode_occluded_host_frame(&mut cache, &screens, &live, &live, &retained, &occlusion)
            .is_empty()
    );
    assert_eq!(cache.host_placements, ids);
    let restore = encode_host_frame(&mut cache, &screens, &live, &live, &retained);
    assert!(!text(&restore).contains("a=t,"));
    assert_eq!(cache.placements.len(), 1);
    assert_eq!(text(&restore).matches("a=d,d=i").count(), 4);
}

#[test]
fn occlusion_leaves_another_image_in_the_same_pane_unchanged() {
    use ratatui::layout::Rect;
    let a = screen(1, visible(0, 0, 0));
    let mut b = screen(1, visible(8, 0, 0));
    b.visible.image_id = 9;
    let input = [a, b];
    let live = HashSet::from([1]);
    let retained = retained_of(&input);
    let mut cache = HostGraphicsCache::default();
    encode_host_frame(&mut cache, &input, &live, &live, &retained);
    let key = HostPlacementKey {
        pane_id: 1,
        image_id: 9,
        placement_id: 3,
        piece: None,
    };
    let id = cache.host_placements[&key];
    let host = cache.host_id(1, 9).unwrap();
    let signature = cache.placements[&(host, id)];
    let mut occlusion = Occlusion::default();
    occlusion.cover(Rect::new(3, 2, 1, 1));
    let changed =
        encode_occluded_host_frame(&mut cache, &input, &live, &live, &retained, &occlusion);
    assert_eq!(cache.host_placements[&key], id);
    assert_eq!(cache.placements[&(host, id)], signature);
    assert!(!text(&changed).contains(&format!("i={host},p={id},")));
    assert!(!text(&changed).contains("a=t,"));
}

#[test]
fn occlusion_upscaled_pixel_still_leaves_visible_pieces() {
    let mut p = screen(1, visible(0, 0, 0));
    p.visible.image_width = 1;
    p.visible.image_height = 1;
    let input = [p];
    let mut occlusion = Occlusion::default();
    occlusion.cover(ratatui::layout::Rect::new(3, 2, 1, 1));
    let pieces = occlusion::frame_pieces(&input, &occlusion);
    assert_eq!(pieces.len(), 4);
    assert!(pieces.iter().all(|(_, c, _)| c.source_x == 0
        && c.source_y == 0
        && c.source_width == 1
        && c.source_height == 1));
}

#[test]
fn occlusion_budget_preserves_untouched_pane_and_replay_backpressure() {
    use ratatui::layout::Rect;
    let mut screens = Vec::new();
    for i in 0..15 {
        let mut p = screen(i + 1, visible(0, 0, 0));
        p.area_x = (i * 5) as u16;
        screens.push(p);
    }
    let mut occlusion = Occlusion::default();
    for p in &screens[..14] {
        occlusion.cover(Rect::new(p.area_x + 1, 2, 1, 1));
    }
    let pieces = occlusion::frame_pieces(&screens, &occlusion);
    assert_eq!(pieces.len(), 1);
    assert_eq!(pieces[0].0.pane_id, 15);
    let mut occlusion = Occlusion::default();
    occlusion.cover(Rect::new(1, 2, 1, 1));
    let live = (1..=15).collect();
    let retained = retained_of(&screens);
    let mut cache = HostGraphicsCache {
        budget_override: 300,
        ..Default::default()
    };
    let original = cache.clone();
    let mut dropped = cache.clone();
    let attempted =
        encode_occluded_host_frame(&mut dropped, &screens, &live, &live, &retained, &occlusion);
    assert_eq!(
        encode_occluded_host_frame(&mut cache, &screens, &live, &live, &retained, &occlusion),
        attempted
    );
    for _ in 0..80 {
        if !cache.remainder() {
            break;
        }
        encode_occluded_host_frame(&mut cache, &screens, &live, &live, &retained, &occlusion);
    }
    assert!(!cache.remainder());
    let expected = cache.placements.clone();
    cache.request_replay();
    for _ in 0..80 {
        encode_occluded_host_frame(&mut cache, &screens, &live, &live, &retained, &occlusion);
        if !cache.remainder() {
            break;
        }
    }
    assert_eq!(cache.placements, expected);
    assert!(original.placements.is_empty());
}

fn rgba(width: u32, height: u32, byte: u8) -> Vec<u8> {
    vec![byte; width as usize * height as usize * 4]
}

fn visible(viewport_col: i32, viewport_row: i32, scroll: u32) -> VisiblePlacement {
    let data = rgba(30, 30, 9);
    let fingerprint = fingerprint(&data);
    VisiblePlacement {
        image_id: 7,
        placement_id: 3,
        z: 0,
        viewport_col,
        viewport_row,
        grid_cols: 3,
        grid_rows: 3,
        pixel_width: 0,
        pixel_height: 0,
        image_width: 30,
        image_height: 30,
        format_code: 32,
        data: Arc::from(data.into_boxed_slice()),
        fingerprint,
        source_x: 0,
        source_y: 0,
        source_width: 0,
        source_height: 0,
        x_offset: 0,
        y_offset: 0,
        scroll_offset: scroll,
    }
}

fn screen(pane: u32, placement: VisiblePlacement) -> ScreenPlacement {
    ScreenPlacement {
        pane_id: pane,
        area_x: 2,
        area_y: 1,
        area_width: 20,
        area_height: 10,
        cell_width_px: 10,
        cell_height_px: 10,
        visible: placement,
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn retained_of(screens: &[ScreenPlacement]) -> HashMap<SourceKey, u64> {
    screens
        .iter()
        .map(|screen| {
            (
                SourceKey {
                    pane_id: screen.pane_id,
                    image_id: screen.visible.image_id,
                },
                screen.visible.fingerprint,
            )
        })
        .collect()
}

fn frame(
    cache: &mut HostGraphicsCache,
    screens: &[ScreenPlacement],
    live: &HashSet<u32>,
) -> Vec<u8> {
    encode_host_frame(cache, screens, live, live, &retained_of(screens))
}

#[test]
fn clipped_placement_crops_negative_viewport_and_keeps_positive_cells() {
    let (inside, _) = clip_placement(&screen(1, visible(2, 2, 0))).unwrap();
    assert_eq!((inside.x, inside.y, inside.cols, inside.rows), (4, 3, 3, 3));
    assert_eq!((inside.source_x, inside.source_y), (0, 0));

    let (cropped, _) = clip_placement(&screen(1, visible(-1, -1, 0))).unwrap();
    assert_eq!(
        (cropped.x, cropped.y, cropped.cols, cropped.rows),
        (2, 1, 2, 2)
    );
    assert_eq!((cropped.source_x, cropped.source_y), (10, 10));
    assert_eq!((cropped.source_width, cropped.source_height), (20, 20));
}

#[test]
fn scroll_offset_change_replaces_placement_without_reuploading() {
    let mut cache = HostGraphicsCache::default();
    let live = HashSet::from([1]);
    let first = frame(&mut cache, &[screen(1, visible(0, 0, 0))], &live);
    let first = text(&first);
    assert!(first.contains("a=t"), "{first}");
    assert!(first.contains("a=p"), "{first}");

    assert!(frame(&mut cache, &[screen(1, visible(0, 0, 0))], &live).is_empty());

    let moved = frame(&mut cache, &[screen(1, visible(0, 4, 3))], &live);
    let moved = text(&moved);
    assert!(moved.contains("a=d,d=i"), "{moved}");
    assert!(moved.contains("a=p"), "{moved}");
    assert!(!moved.contains("a=t"), "{moved}");
    assert!(moved.contains("\x1b[6;3H"), "{moved}");
}

#[test]
fn pane_image_ids_are_isolated_and_close_deletes_only_that_pane() {
    let mut cache = HostGraphicsCache::default();
    let live = HashSet::from([1, 2]);
    let uploaded = frame(
        &mut cache,
        &[screen(1, visible(0, 0, 0)), screen(2, visible(1, 1, 0))],
        &live,
    );
    let uploaded = text(&uploaded);
    let id_one = cache.host_id(1, 7).unwrap();
    let id_two = cache.host_id(2, 7).unwrap();
    assert_ne!(id_one, id_two);
    assert!(uploaded.contains(&format!("i={id_one}")));
    assert!(uploaded.contains(&format!("i={id_two}")));

    let closed = frame(
        &mut cache,
        &[screen(2, visible(1, 1, 0))],
        &HashSet::from([2]),
    );
    let closed = text(&closed);
    assert!(closed.contains(&format!("a=d,d=I,i={id_one}")), "{closed}");
    assert!(!closed.contains(&format!("i={id_two}")), "{closed}");
    assert!(cache.sources.contains_key(&SourceKey {
        pane_id: 2,
        image_id: 7
    }));
    assert!(!cache.sources.contains_key(&SourceKey {
        pane_id: 1,
        image_id: 7
    }));
}

#[test]
fn placeholder_characters_are_ordinary_single_width_spaces() {
    assert_eq!(host_cell_symbol("\u{10EEEE}"), " ");
    assert_eq!(host_cell_symbol("\u{10EEEE}\u{0305}\u{030d}"), " ");
    assert_eq!(host_cell_symbol("A"), "A");
    assert_eq!(
        unicode_width::UnicodeWidthStr::width(host_cell_symbol("\u{10EEEE}")),
        1
    );

    let mut plain = Vec::new();
    let mut swallow = false;
    emit_plain("A\u{10EEEE}\u{0305}B".as_bytes(), &mut plain, &mut swallow);
    assert_eq!(plain, b"A B");
    assert!(!plain.windows(4).any(|bytes| bytes == PLACEHOLDER));
}

#[test]
fn direct_placement_parses_without_leaking_the_escape_into_text() {
    let pixels = rgba(1, 1, 7);
    let mut payload = Vec::new();
    base64_encode(&pixels, &mut payload);
    let mut command = b"\x1b_Ga=T,f=32,s=1,v=1,c=1,r=1,i=4,q=2;".to_vec();
    command.extend_from_slice(&payload);
    command.extend_from_slice(b"\x1b\\");
    let mut graphics = PaneGraphics::default();
    let mut plain = Vec::new();
    plain.extend_from_slice(b"before");
    let actions = graphics.push(&command, &mut plain);
    plain.extend_from_slice(b"after");
    assert_eq!(plain, b"beforeafter");
    assert!(matches!(actions.as_slice(), [GraphicsAction::Place(_)]));
}

#[test]
fn placement_scrolls_with_its_row_and_leaves_width_one_cells() {
    use crate::terminal::vt::VtEngine;

    let (tx, _rx) = std::sync::mpsc::channel();
    let mut engine = crate::terminal::vt::alacritty::AlacrittyEngine::new(20, 5, tx, 64 * 1024);
    let pixels = rgba(1, 1, 7);
    let mut payload = Vec::new();
    base64_encode(&pixels, &mut payload);
    let mut command = b"before \x1b_Ga=T,f=32,s=1,v=1,C=1,q=2;".to_vec();
    command.extend_from_slice(&payload);
    command.extend_from_slice(b"\x1b\\ after\n");
    engine.advance(&command);
    engine.finish_output_batch();

    let placed = engine.kitty_visible_placements(10, 20);
    assert_eq!(placed.len(), 1);
    assert_eq!(placed[0].viewport_row, 0);
    let first = engine.screen_rows().remove(0);
    assert!(first.starts_with("before  after"), "{first}");
    assert!(!first.contains('\u{10EEEE}'));
    assert_eq!(
        unicode_width::UnicodeWidthStr::width(first.trim_end()),
        "before  after".len()
    );

    engine.advance(b"\n\n\n\n\n");
    engine.finish_output_batch();
    let scrolled = engine.kitty_visible_placements(10, 20);
    assert_eq!(scrolled.len(), 1);
    assert!(
        scrolled[0].viewport_row < 0,
        "{:?}",
        scrolled[0].viewport_row
    );
    engine.scroll(-scrolled[0].viewport_row);
    let restored = engine.kitty_visible_placements(10, 20);
    assert_eq!(restored[0].viewport_row, 0);
    for row in engine.screen_rows() {
        assert!(!row.contains('\u{10EEEE}'));
    }
}

#[test]
fn query_reports_only_supported_transfer_and_honors_quiet_without_mutation() {
    let mut graphics = PaneGraphics::default();
    let mut plain = Vec::new();
    for mode in [
        "", ",t=d", ",t=f", ",t=s", ",t=t", ",t=x", ",o=z", ",o=x", ",f=8",
    ] {
        for quiet in 0..=2 {
            let supported = matches!(mode, "" | ",t=d");
            let query = format!("\x1b_Ga=q,i=37,q={quiet}{mode};AAAA\x1b\\");
            let actions = graphics.push(query.as_bytes(), &mut plain);
            let replies: Vec<_> = actions
                .iter()
                .filter_map(|a| match a {
                    GraphicsAction::Reply(bytes) => Some(text(bytes)),
                    _ => None,
                })
                .collect();
            let expected = if supported { quiet == 0 } else { quiet < 2 };
            assert_eq!(replies.len(), usize::from(expected), "{mode} q={quiet}");
            if expected {
                assert!(replies[0].contains(if supported { ";OK" } else { ";ENOTSUPPORTED" }));
            }
            assert!(graphics.images.is_empty());
            assert!(graphics.placements.is_empty());
        }
    }
    let actions = graphics.push(b"\x1b_Ga=q;AAAA\x1b\\", &mut plain);
    assert!(actions
        .iter()
        .all(|a| !matches!(a, GraphicsAction::Reply(_))));
}

#[test]
fn pane_clip_and_overlay_share_offset_aware_source_boundaries() {
    use ratatui::layout::Rect;
    for (col, row) in [(-1, -1), (0, 0), (2, 2)] {
        for (offset_x, offset_y) in [(0, 5), (5, 0), (5, 5)] {
            for (source_x, source_y, width, height) in [(0, 0, 100, 100), (10, 20, 70, 60)] {
                for cells in [5, 10, 20] {
                    let mut p = screen(1, visible(col, row, 0));
                    p.area_width = 30;
                    p.area_height = 30;

                    p.visible.image_width = 100;
                    p.visible.image_height = 100;
                    p.visible.source_x = source_x;
                    p.visible.source_y = source_y;
                    p.visible.source_width = width;
                    p.visible.source_height = height;
                    p.visible.grid_cols = cells;
                    p.visible.grid_rows = cells;
                    p.visible.x_offset = offset_x;
                    p.visible.y_offset = offset_y;
                    let whole = clip_placement(&p).unwrap().0;
                    let rect = Rect::new(whole.x, whole.y, whole.cols as u16, whole.rows as u16);
                    let crop = occlusion::crop_piece(&p, rect).unwrap();
                    assert_eq!(
                        (
                            whole.source_x,
                            whole.source_y,
                            whole.source_width,
                            whole.source_height
                        ),
                        (
                            crop.source_x,
                            crop.source_y,
                            crop.source_width,
                            crop.source_height
                        )
                    );
                    let mut cover = Occlusion::default();
                    cover.cover(Rect::new(rect.x, rect.y + rect.height / 2, rect.width, 1));
                    let input = [p.clone()];
                    let pieces = occlusion::frame_pieces(&input, &cover);
                    let top = pieces
                        .iter()
                        .find(|(_, piece, _)| piece.y == rect.y)
                        .unwrap()
                        .1;
                    assert_eq!(
                        (top.source_x, top.source_y),
                        (whole.source_x, whole.source_y)
                    );
                    let boundary = top.source_y + top.source_height;
                    let lower = occlusion::crop_piece(
                        &p,
                        Rect::new(rect.x, top.y + top.rows as u16, rect.width, 1),
                    )
                    .unwrap();
                    assert_eq!(boundary, lower.source_y);
                    let no_cover = occlusion::frame_pieces(&input, &Occlusion::default());
                    assert_eq!(no_cover[0].1.source_y, whole.source_y);
                }
            }
        }
    }
    let mut p = screen(1, visible(0, -1, 0));
    p.visible.image_width = 100;
    p.visible.image_height = 100;
    p.visible.grid_cols = 10;
    p.visible.grid_rows = 10;
    p.visible.y_offset = 5;
    assert_eq!(clip_placement(&p).unwrap().0.source_y, 5);
}

#[test]
fn feature_query_does_not_advertise_unicode_placeholders() {
    let mut graphics = PaneGraphics::default();
    let mut plain = Vec::new();
    let actions = graphics.push(b"\x1b_Ga=q,i=31,e=T,U=1,C=1\x1b\\", &mut plain);
    match actions.as_slice() {
        [GraphicsAction::Reply(bytes)] => {
            let reply = text(bytes);
            assert!(reply.contains("e=T"), "{reply}");
            assert!(reply.contains("C=1"), "{reply}");
            assert!(!reply.contains("U=1"), "{reply}");
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(plain.is_empty());
}

#[test]
fn child_query_is_refused_until_the_host_agrees() {
    let mut graphics = PaneGraphics::default();
    graphics.set_queries_enabled(false);
    let mut plain = Vec::new();
    let actions = graphics.push(b"\x1b_Ga=q,i=31,e=T,C=1\x1b\\", &mut plain);
    match actions.as_slice() {
        [GraphicsAction::Reply(bytes)] => {
            let reply = text(bytes);
            assert!(reply.contains("ENOTSUPPORTED"), "{reply}");
            assert!(!reply.contains("OK"), "{reply}");
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(!cell_size_query_required(false, None));
    assert!(!cell_size_query_required(true, Some((8, 16))));
    assert_eq!(cell_size_query_required(true, None), !cfg!(windows));
}

#[test]
fn host_query_follows_the_display_flag() {
    use crate::terminal::vt::VtEngine;
    use std::sync::atomic::{AtomicBool, Ordering};

    let flag = Arc::new(AtomicBool::new(false));
    install_host_draw(flag.clone());
    let (tx, rx) = std::sync::mpsc::channel();
    let mut engine = crate::terminal::vt::alacritty::AlacrittyEngine::new(20, 5, tx, 64 * 1024);
    let query = b"\x1b_Ga=q,i=31\x1b\\";
    engine.advance(query);
    engine.finish_output_batch();
    let refused = text(&input_bytes(rx.try_recv().unwrap()));
    assert!(refused.contains("ENOTSUPPORTED"), "{refused}");
    flag.store(true, Ordering::Release);
    engine.advance(query);
    engine.finish_output_batch();
    let accepted = text(&input_bytes(rx.try_recv().unwrap()));
    assert!(accepted.contains("OK"), "{accepted}");
    flag.store(false, Ordering::Release);
}

#[test]
fn unsupported_animation_preserves_base_image_for_a_later_client() {
    use crate::terminal::vt::VtEngine;
    let (tx, rx) = std::sync::mpsc::channel();
    let mut engine = crate::terminal::vt::alacritty::AlacrittyEngine::new(20, 5, tx, 64 * 1024);
    let first = rgba(1, 1, 1);
    let second = rgba(1, 1, 2);
    engine.advance(&transmit(b'a', b'T', 4, &first, false));
    engine.finish_output_batch();
    let frame_command = transmit(b'a', b'f', 4, &second, false);
    let frame_command = String::from_utf8(frame_command)
        .unwrap()
        .replace("q=2", "q=0");
    engine.advance(frame_command.as_bytes());
    let reply = text(&input_bytes(rx.try_recv().unwrap()));
    assert!(reply.contains("ENOTSUPPORTED"), "{reply}");
    engine.finish_output_batch();
    let placed = engine.kitty_visible_placements(10, 20);
    assert_eq!(placed.len(), 1);
    assert_eq!(&*placed[0].data, first.as_slice());

    let mut cache = HostGraphicsCache::default();
    let replay = frame(
        &mut cache,
        &[screen(1, placed[0].clone())],
        &HashSet::from([1]),
    );
    let replay = text(&replay);
    assert!(replay.contains("a=t"), "{replay}");
    let mut encoded = Vec::new();
    base64_encode(&first, &mut encoded);
    assert!(
        replay.contains(std::str::from_utf8(&encoded).unwrap()),
        "{replay}"
    );
}

#[test]
fn interrupted_or_oversized_chunks_are_not_placed() {
    let mut graphics = PaneGraphics::default();
    let mut plain = Vec::new();
    let start = b"\x1b_Ga=T,f=32,s=1,v=1,i=9,m=1;AAAA\x1b\\";
    assert!(graphics.push(start, &mut plain).is_empty());
    let interrupted = graphics.push(b"\x1b_Ga=q,i=1\x1b\\", &mut plain);
    assert!(interrupted
        .iter()
        .all(|action| !matches!(action, GraphicsAction::Place(_))));
    let place = graphics.push(b"\x1b_Ga=p,i=9,c=1,r=1\x1b\\", &mut plain);
    assert!(
        matches!(place.as_slice(), [GraphicsAction::Reply(bytes)] if text(bytes).contains("ENOENT")),
        "{place:?}"
    );

    graphics.set_image_limit(4);
    assert!(graphics
        .push(b"\x1b_Ga=T,f=32,s=1,v=1,i=8,m=1;AAAA\x1b\\", &mut plain)
        .is_empty());
    let overflow = graphics.push(b"\x1b_Gm=0;AAAA\x1b\\", &mut plain);
    assert!(
        matches!(overflow.as_slice(), [GraphicsAction::Reply(bytes)] if text(bytes).contains("EBADF")),
        "{overflow:?}"
    );
    let place = graphics.push(b"\x1b_Ga=p,i=8,c=1,r=1\x1b\\", &mut plain);
    assert!(
        matches!(place.as_slice(), [GraphicsAction::Reply(bytes)] if text(bytes).contains("ENOENT")),
        "{place:?}"
    );
}

#[test]
fn unknown_cell_size_does_not_place() {
    let mut placement = screen(1, visible(0, 0, 0));
    placement.cell_width_px = 0;
    placement.cell_height_px = 0;
    let bytes = frame(
        &mut HostGraphicsCache::default(),
        &[placement],
        &HashSet::from([1]),
    );
    assert!(bytes.is_empty());
}

#[test]
fn oversized_placement_is_rejected_without_a_huge_cursor_move() {
    let mut graphics = PaneGraphics::default();
    let mut plain = Vec::new();
    let pixels = rgba(1, 1, 3);
    let mut payload = Vec::new();
    base64_encode(&pixels, &mut payload);
    let mut command = b"\x1b_Ga=T,f=32,s=1,v=1,i=4,c=1000000,r=1000000,q=1;".to_vec();
    command.extend_from_slice(&payload);
    command.extend_from_slice(b"\x1b\\");
    let actions = graphics.push(&command, &mut plain);
    assert!(
        actions
            .iter()
            .all(|action| !matches!(action, GraphicsAction::Place(_))),
        "{actions:?}"
    );
    assert!(
            actions.iter().any(|action| matches!(action, GraphicsAction::Reply(bytes) if text(bytes).contains("EINVAL"))),
            "{actions:?}"
        );
}

#[test]
fn image_dimensions_do_not_overflow() {
    let mut graphics = PaneGraphics::default();
    let mut plain = Vec::new();
    let actions = graphics.push(
        b"\x1b_Ga=T,f=32,s=65535,v=65535,i=4,q=1;AAAA\x1b\\",
        &mut plain,
    );
    assert!(
        actions
            .iter()
            .all(|action| !matches!(action, GraphicsAction::Place(_))),
        "{actions:?}"
    );
}

#[test]
fn quiet_defaults_to_a_reply() {
    let mut graphics = PaneGraphics::default();
    let mut plain = Vec::new();
    let mut payload = Vec::new();
    base64_encode(&rgba(1, 1, 4), &mut payload);
    let mut command = b"\x1b_Ga=T,f=32,s=1,v=1,i=3,c=1,r=1;".to_vec();
    command.extend_from_slice(&payload);
    command.extend_from_slice(b"\x1b\\");
    let actions = graphics.push(&command, &mut plain);
    assert!(
        actions.iter().any(
            |action| matches!(action, GraphicsAction::Reply(bytes) if text(bytes).contains("OK"))
        ),
        "{actions:?}"
    );
}

#[test]
fn explicit_and_automatic_placement_ids_do_not_collide() {
    let mut graphics = PaneGraphics::default();
    let mut plain = Vec::new();
    let mut payload = Vec::new();
    base64_encode(&rgba(1, 1, 5), &mut payload);
    let mut first = b"\x1b_Ga=T,f=32,s=1,v=1,i=5,p=1,c=1,r=1,q=2;".to_vec();
    first.extend_from_slice(&payload);
    first.extend_from_slice(b"\x1b\\");
    for action in graphics.push(&first, &mut plain) {
        if let GraphicsAction::Place(placement) = action {
            graphics.record(1, 0, placement);
        }
    }
    let mut second = b"\x1b_Ga=p,i=5,c=1,r=1,q=2;".to_vec();
    second.extend_from_slice(b"\x1b\\");
    for action in graphics.push(&second, &mut plain) {
        if let GraphicsAction::Place(placement) = action {
            graphics.record(1, 0, placement);
        }
    }
    let ids: Vec<_> = graphics
        .placements
        .iter()
        .map(|placement| (placement.child_placement_id, placement.local_placement_id))
        .collect();
    assert_eq!(ids.len(), 2, "{ids:?}");
    assert_ne!(ids[0].1, ids[1].1, "{ids:?}");
}

#[test]
fn shrunk_image_that_fits_is_not_cropped() {
    let mut placement = visible(0, 0, 0);
    placement.image_width = 300;
    placement.image_height = 300;
    placement.grid_cols = 2;
    placement.grid_rows = 2;
    placement.pixel_width = 300;
    placement.pixel_height = 300;
    placement.x_offset = 15;
    let bytes = text(&frame(
        &mut HostGraphicsCache::default(),
        &[screen(1, placement)],
        &HashSet::from([1]),
    ));
    assert!(!bytes.contains(",w="), "{bytes}");
    assert!(bytes.contains(",X=15"), "{bytes}");
}

#[test]
fn dropped_frame_uploads_again_and_replay_sends_pixels() {
    let mut cache = HostGraphicsCache::default();
    let live = HashSet::from([1]);
    let screens = [screen(1, visible(0, 0, 0))];
    let mut staged = cache.clone();
    let first = text(&frame(&mut staged, &screens, &live));
    assert!(first.contains("a=t"), "{first}");
    let retry = text(&frame(&mut cache, &screens, &live));
    assert!(retry.contains("a=t"), "{retry}");

    frame(&mut cache, &screens, &live);
    cache.request_replay();
    let again = text(&frame(&mut cache, &screens, &live));
    assert!(again.contains("a=t"), "{again}");
}

#[test]
fn evicting_an_image_deletes_its_host_record() {
    let mut cache = HostGraphicsCache::default();
    let live = HashSet::from([1]);
    frame(&mut cache, &[screen(1, visible(0, 0, 0))], &live);
    assert!(cache.host_id(1, 7).is_some());
    let gone = text(&encode_host_frame(
        &mut cache,
        &[],
        &live,
        &live,
        &HashMap::new(),
    ));
    assert!(gone.contains("a=d,d=I"), "{gone}");
    assert!(cache.host_id(1, 7).is_none());
}

#[test]
fn large_images_split_across_frames() {
    let mut cache = HostGraphicsCache {
        budget_override: 700,
        ..HostGraphicsCache::default()
    };
    let mut first_image = visible(0, 0, 0);
    first_image.data = Arc::from(vec![1u8; 120].into_boxed_slice());
    first_image.fingerprint = fingerprint(&first_image.data);
    let mut second_image = visible(1, 1, 0);
    second_image.image_id = 8;
    second_image.data = Arc::from(vec![2u8; 120].into_boxed_slice());
    second_image.fingerprint = fingerprint(&second_image.data);
    let screens = [screen(1, first_image), screen(1, second_image)];
    let live = HashSet::from([1]);
    let first = text(&frame(&mut cache, &screens, &live));
    assert!(cache.remainder(), "{first}");
    assert_eq!(first.matches("a=t").count(), 1, "{first}");
    let second = text(&frame(&mut cache, &screens, &live));
    assert!(second.contains("a=t"), "{second}");
}

#[test]
fn host_placement_ids_are_retired_and_exhaustion_is_bounded() {
    let mut cache = HostGraphicsCache {
        placement_id_limit: 2,
        ..Default::default()
    };
    let live = HashSet::from([1]);
    for id in 1..100 {
        let mut image = visible(0, 0, 0);
        image.placement_id = id;
        frame(&mut cache, &[screen(1, image)], &live);
        assert_eq!(cache.host_placements.len(), 1);
    }
    let mut images = vec![screen(1, visible(0, 0, 0)); 3];
    for (i, p) in images.iter_mut().enumerate() {
        p.visible.placement_id = i as u32 + 100;
    }
    frame(&mut cache, &images, &live);
    assert_eq!(cache.host_placements.len(), 2);
    assert_eq!(allocate_host_placement(&mut cache), None);
}

#[test]
fn placeholder_utf8_and_marks_split_at_every_byte() {
    let text = "A\u{10EEEE}\u{0301}B".as_bytes();
    for split in 1..text.len() {
        let mut parser = GraphicsParser::default();
        let mut plain = Vec::new();
        for bytes in [&text[..split], &text[split..]] {
            for event in parser.drain(bytes) {
                if let StreamEvent::Plain(bytes) = event {
                    plain.extend(bytes);
                }
            }
        }
        assert_eq!(plain, b"A B", "split {split}");
    }
}

#[test]
fn passive_projection_does_not_change_runtime_cell_size() {
    use crate::terminal::vt::VtEngine;
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut engine = crate::terminal::vt::alacritty::AlacrittyEngine::new(20, 6, tx, 64 * 1024);
    // Foreground has 10px cells, passive display has 1px cells.
    engine.set_runtime_cell_pixels(10, 10);
    engine.kitty_visible_placements(10, 10);
    engine.kitty_visible_placements(1, 1);
    let pixels = rgba(20, 10, 3);
    let mut payload = Vec::new();
    base64_encode(&pixels, &mut payload);
    let mut command = b"\x1b_Ga=T,f=32,s=20,v=10,i=4,q=2;".to_vec();
    command.extend_from_slice(&payload);
    command.extend_from_slice(b"\x1b\\");
    engine.advance(&command);
    assert_eq!((engine.cursor().x, engine.cursor().y), (2, 1));
}

#[test]
fn placeholder_marks_from_the_next_read_do_not_stick() {
    let mut parser = GraphicsParser::default();
    let first = parser.drain("\u{10EEEE}".as_bytes());
    let second = parser.drain("\u{0301}X".as_bytes());
    let plain = |events: Vec<StreamEvent>| {
        events
            .into_iter()
            .filter_map(|event| match event {
                StreamEvent::Plain(bytes) => Some(bytes),
                StreamEvent::Command(_) => None,
            })
            .flatten()
            .collect::<Vec<_>>()
    };
    assert_eq!(plain(first), b" ");
    assert_eq!(plain(second), b"X");
}

#[test]
fn sync_update_places_at_the_cursor_the_update_moves_to() {
    use crate::terminal::vt::VtEngine;
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut engine = crate::terminal::vt::alacritty::AlacrittyEngine::new(20, 6, tx, 64 * 1024);
    engine.kitty_visible_placements(10, 10);
    let mut command = b"\x1b[?2026h\x1b[3;4H".to_vec();
    command.extend_from_slice(&transmit(b'a', b'T', 4, &rgba(1, 1, 6), false));
    command.extend_from_slice(b"\x1b[?2026l");
    engine.advance(&command);
    engine.finish_output_batch();
    let placed = engine.kitty_visible_placements(10, 10);
    assert_eq!(placed.len(), 1, "image missing");
    assert_eq!((placed[0].viewport_col, placed[0].viewport_row), (3, 2));
}

#[test]
fn origin_mode_does_not_add_the_scroll_region_twice() {
    use crate::terminal::vt::VtEngine;
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut engine = crate::terminal::vt::alacritty::AlacrittyEngine::new(20, 10, tx, 64 * 1024);
    engine.advance(b"\x1b[3;8r\x1b[?6h");
    let before = engine.cursor();
    let mut payload = Vec::new();
    base64_encode(&rgba(1, 1, 7), &mut payload);
    let mut command = b"\x1b_Ga=T,f=32,s=1,v=1,c=1,r=1,i=4,q=2;".to_vec();
    command.extend_from_slice(&payload);
    command.extend_from_slice(b"\x1b\\");
    engine.advance(&command);
    let after = engine.cursor();
    assert_eq!(
        after.y,
        before.y + 1,
        "cursor row jumped by the origin twice"
    );
    assert_eq!(after.x, before.x.saturating_add(1));
}

#[test]
fn cursor_delete_hits_a_middle_row_of_a_tall_image() {
    use crate::terminal::vt::VtEngine;
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut engine = crate::terminal::vt::alacritty::AlacrittyEngine::new(20, 8, tx, 64 * 1024);
    let mut payload = Vec::new();
    base64_encode(&rgba(1, 1, 8), &mut payload);
    let mut command = b"\x1b_Ga=T,f=32,s=1,v=1,c=1,r=3,i=4,C=1,q=2;".to_vec();
    command.extend_from_slice(&payload);
    command.extend_from_slice(b"\x1b\\");
    engine.advance(&command);
    engine.advance(b"\n\x1b_Ga=d,d=c,q=2;\x1b\\");
    engine.finish_output_batch();
    assert!(
        engine.kitty_visible_placements(10, 10).is_empty(),
        "cursor delete missed the image"
    );
}

#[test]
fn tall_image_stays_visible_below_a_fixed_lookback() {
    use crate::terminal::vt::VtEngine;
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut engine =
        crate::terminal::vt::alacritty::AlacrittyEngine::new(20, 5, tx, 8 * 1024 * 1024);
    let mut payload = Vec::new();
    base64_encode(&rgba(1, 1, 9), &mut payload);
    let mut command = b"\x1b_Ga=T,f=32,s=1,v=1,c=1,r=400,i=4,C=1,q=2;".to_vec();
    command.extend_from_slice(&payload);
    command.extend_from_slice(b"\x1b\\");
    engine.advance(&command);
    engine.advance(&vec![b'\n'; 300]);
    engine.finish_output_batch();
    assert_eq!(engine.kitty_visible_placements(10, 20).len(), 1);
}

#[test]
fn reflow_keeps_the_image_with_the_wrapped_text() {
    use crate::terminal::vt::VtEngine;
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut engine = crate::terminal::vt::alacritty::AlacrittyEngine::new(8, 6, tx, 64 * 1024);
    engine.advance(b"01234567WRAP");
    engine.advance(&transmit(b'a', b'T', 4, &rgba(1, 1, 2), false));
    engine.finish_output_batch();
    assert_eq!(engine.kitty_visible_placements(10, 10).len(), 1);
    engine.resize(30, 6);
    engine.finish_output_batch();
    assert_eq!(
        engine.kitty_visible_placements(10, 10).len(),
        1,
        "widen dropped the anchor"
    );
    engine.resize(8, 6);
    engine.finish_output_batch();
    assert_eq!(
        engine.kitty_visible_placements(10, 10).len(),
        1,
        "narrow dropped the anchor"
    );
}

#[test]
fn cursor_delete_resolves_omitted_columns_from_runtime_pixels() {
    use crate::terminal::vt::VtEngine;
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut e = crate::terminal::vt::alacritty::AlacrittyEngine::new(20, 6, tx, 64 * 1024);
    e.set_runtime_cell_pixels(10, 10);
    let mut payload = Vec::new();
    base64_encode(&rgba(30, 10, 3), &mut payload);
    let mut command = b"\x1b_Ga=T,f=32,s=30,v=10,i=4,C=1,q=2;".to_vec();
    command.extend(payload);
    command.extend_from_slice(b"\x1b\\\x1b[2G\x1b_Ga=d,d=c,q=2;\x1b\\");
    e.advance(&command);
    assert!(e.kitty_visible_placements(10, 10).is_empty());
}

#[test]
fn omitted_cell_size_moves_the_cursor_from_image_pixels() {
    use crate::terminal::vt::VtEngine;
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut engine = crate::terminal::vt::alacritty::AlacrittyEngine::new(20, 6, tx, 64 * 1024);
    engine.set_runtime_cell_pixels(10, 10);
    let pixels = rgba(20, 10, 3);
    let mut payload = Vec::new();
    base64_encode(&pixels, &mut payload);
    let mut command = b"\x1b_Ga=T,f=32,s=20,v=10,i=4,q=2;".to_vec();
    command.extend_from_slice(&payload);
    command.extend_from_slice(b"\x1b\\");
    engine.advance(&command);
    let cursor = engine.cursor();
    assert_eq!((cursor.x, cursor.y), (2, 1));
}

fn transmit(key: u8, action: u8, id: u32, pixels: &[u8], more: bool) -> Vec<u8> {
    let mut payload = Vec::new();
    base64_encode(pixels, &mut payload);
    let more = if more { ",m=1" } else { "" };
    let mut command = format!(
        "\x1b_G{}={},f=32,s=1,v=1,c=1,r=1,i={id},C=1,q=2{more};",
        key as char, action as char
    )
    .into_bytes();
    command.extend_from_slice(&payload);
    command.extend_from_slice(b"\x1b\\");
    command
}

fn input_bytes(action: crate::terminal::pty::InputAction) -> Vec<u8> {
    match action {
        crate::terminal::pty::InputAction::Bytes(bytes) => bytes,
        crate::terminal::pty::InputAction::Submit { .. } => Vec::new(),
    }
}
