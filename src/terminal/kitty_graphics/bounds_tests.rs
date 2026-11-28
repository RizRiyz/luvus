use super::*;

fn png(width: u32, height: u32) -> Vec<u8> {
    crate::clipboard_image::encode_rgba_png(width, height, |_, _| [1, 2, 3, 255]).unwrap()
}
fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = (data.len() as u32).to_be_bytes().to_vec();
    out.extend(kind);
    out.extend(data);
    let mut crc = u32::MAX;
    for byte in &out[4..] {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    out.extend((!crc).to_be_bytes());
    out
}
fn header_png(width: u32, height: u32) -> Vec<u8> {
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = width.to_be_bytes().to_vec();
    header.extend(height.to_be_bytes());
    header.extend([8, 6, 0, 0, 0]);
    out.extend(chunk(b"IHDR", &header));
    out.extend(chunk(b"IDAT", b"not inflated here"));
    out.extend(chunk(b"IEND", b""));
    out
}
fn command(keys: &str, payload: Vec<u8>) -> ParsedCommand {
    ParsedCommand {
        entries: keys
            .split(',')
            .map(|entry| {
                let (k, v) = entry.split_once('=').unwrap();
                (k.chars().next().unwrap(), v.into())
            })
            .collect(),
        payload,
    }
}

#[test]
fn png_container_rejects_truncation_corruption_and_surface_bombs_without_fallback() {
    let valid = png(2, 3);
    assert_eq!(png_size(&valid), Some((2, 3)));
    for end in 0..valid.len() {
        assert!(png_size(&valid[..end]).is_none(), "prefix {end}");
    }
    let mut cases = vec![
        valid[..24].to_vec(),
        header_png(u32::MAX, u32::MAX),
        header_png(4097, 1),
        header_png(2048, 2048),
        header_png(0, 1),
    ];
    let mut overflowing_chunk = valid[..33].to_vec();
    overflowing_chunk.extend(u32::MAX.to_be_bytes());
    overflowing_chunk.extend(b"IDAT");
    cases.push(overflowing_chunk);
    let mut empty_idat = valid[..33].to_vec();
    empty_idat.extend(chunk(b"IDAT", b""));
    empty_idat.extend(chunk(b"IEND", b""));
    cases.push(empty_idat);
    let mut duplicate_header = valid[..33].to_vec();
    duplicate_header.extend(&valid[8..]);
    cases.push(duplicate_header);
    let mut bad_crc = valid.clone();
    bad_crc[30] ^= 1;
    cases.push(bad_crc);
    let mut trailing = valid.clone();
    trailing.push(0);
    cases.push(trailing);
    let mut reordered = valid[..33].to_vec();
    reordered.extend(chunk(b"IDAT", b"x"));
    reordered.extend(chunk(b"teXt", b"x"));
    reordered.extend(chunk(b"IDAT", b"x"));
    reordered.extend(chunk(b"IEND", b""));
    cases.push(reordered);
    for kind in [b"acTL", b"ABCD", b"abcD"] {
        let mut data = valid[..33].to_vec();
        data.extend(chunk(kind, b"x"));
        data.extend(&valid[33..]);
        cases.push(data);
    }
    for data in cases {
        assert!(png_size(&data).is_none());
        let mut graphics = PaneGraphics::default();
        let actions = graphics.apply(command("a=T,f=100,i=37,s=1,v=1", data));
        assert!(
            matches!(actions.as_slice(),[GraphicsAction::Reply(bytes)] if String::from_utf8_lossy(bytes).contains("i=37;EINVAL"))
        );
        assert!(graphics.images.is_empty());
        assert!(graphics.placements.is_empty());
    }
    // 16-bit RGBA has a larger scanline allocation than its RGBA8 surface.
    let wide = header_png(1024, 1024);
    let mut header = wide[16..29].to_vec();
    header[8] = 16;
    let mut sixteen = wide[..8].to_vec();
    sixteen.extend(chunk(b"IHDR", &header));
    sixteen.extend(&wide[33..]);
    assert!(png_size(&sixteen).is_none());
    let mut graphics = PaneGraphics::default();
    assert!(graphics
        .apply(command("a=T,f=100,i=37,s=999,v=999", valid))
        .iter()
        .any(|a| matches!(a, GraphicsAction::Place(_))));
    assert_eq!(
        (graphics.images[&37].width, graphics.images[&37].height),
        (2, 3)
    );
}

#[test]
fn natural_explicit_crop_cursor_and_lookback_share_bounded_spans() {
    let mut graphics = PaneGraphics::default();
    for (cols, rows, crop) in [
        (0, 0, ""),
        (2, 3, ""),
        (0, 0, ",x=10,y=2,w=20,h=3"),
        (4, 0, ",x=10,y=2,w=20,h=3"),
    ] {
        graphics.placements.clear();
        let actions = graphics.apply(command(
            &format!("a=T,f=100,i=37,c={cols},r={rows}{crop}"),
            png(100, 10),
        ));
        let placement = actions
            .into_iter()
            .find_map(|a| {
                if let GraphicsAction::Place(p) = a {
                    Some(p)
                } else {
                    None
                }
            })
            .unwrap();
        let cursor = graphics.cursor_span(&placement, 1, 1).unwrap();
        graphics.record(1, 0, placement);
        let visible = graphics.visible(&[(1, 0)], 1, 1, 0);
        let v = visible.last().unwrap();
        assert_eq!((v.grid_cols, v.grid_rows), cursor);
        assert_eq!(graphics.lookback_rows(1), i32::try_from(cursor.1).unwrap());
        assert!(cursor.0 <= MAX_PLACEMENT_CELLS && cursor.1 <= MAX_PLACEMENT_CELLS);
    }
    // Maximum natural dimension at minimum cell size is accepted consistently,
    // not clamped only for the cursor. Oversized dimensions fail even if scaled.
    let actions = graphics.apply(command("a=T,f=100,i=38", png(4096, 1)));
    let placement = actions
        .into_iter()
        .find_map(|a| {
            if let GraphicsAction::Place(p) = a {
                Some(p)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(graphics.cursor_span(&placement, 1, 1), Some((4096, 1)));
    graphics.record(2, 0, placement);
    assert_eq!(graphics.visible(&[(2, 0)], 1, 1, 0)[0].grid_cols, 4096);
    let actions = graphics.apply(command("a=T,f=100,i=41", png(1, 4096)));
    let placement = actions
        .into_iter()
        .find_map(|a| {
            if let GraphicsAction::Place(p) = a {
                Some(p)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(graphics.cursor_span(&placement, 1, 1), Some((1, 4096)));
    graphics.record(3, 0, placement);
    assert_eq!(graphics.lookback_rows(1), 4096);
    assert_eq!(graphics.visible(&[(3, 0)], 1, 1, 0)[0].grid_rows, 4096);
    assert!(!bounded_image_size(u32::MAX, u32::MAX));
    assert!(graphics
        .apply(command(
            "a=T,f=24,i=40,s=4097,v=1,c=1,r=1",
            vec![0; 4097 * 3]
        ))
        .iter()
        .all(|a| !matches!(a, GraphicsAction::Place(_))));
    assert_eq!(rgb_byte_len(u32::MAX, u32::MAX, 4), None);
    let actions = graphics.apply(command("a=T,f=100,i=39,c=1,r=1", header_png(4097, 1)));
    assert!(!actions
        .iter()
        .any(|a| matches!(a, GraphicsAction::Place(_))));
}

#[test]
fn chunk_overflow_preserves_original_identity_quiet_and_recovers() {
    for quiet in 0..=2 {
        let mut graphics = PaneGraphics::default();
        // Two chunks total 9MiB at the production cap, no parser/base64 timing
        // dependence. Continuation q cannot silently replace original policy.
        assert!(graphics
            .apply(command(
                &format!("a=T,f=32,s=1,v=1,i=37,q={quiet},m=1"),
                vec![0; 4 * 1024 * 1024]
            ))
            .is_empty());
        let actions = graphics.apply(command("m=0,q=0", vec![0; 5 * 1024 * 1024]));
        if quiet == 2 {
            assert!(actions.is_empty());
        } else {
            assert!(
                matches!(actions.as_slice(),[GraphicsAction::Reply(bytes)] if bytes==b"\x1b_Gi=37;EBADF\x1b\\")
            );
        }
        assert!(graphics.pending.is_none());
        assert!(graphics.images.is_empty());
        assert!(graphics.placements.is_empty());
        assert!(graphics.apply(command("m=0", vec![0; 4])).is_empty());
        let recovered = graphics.apply(command("a=T,f=32,s=1,v=1,i=38,q=2", vec![0; 4]));
        assert!(matches!(recovered.as_slice(), [GraphicsAction::Place(_)]));
        assert!(graphics.images.contains_key(&38));
    }
}
