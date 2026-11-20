//! Bounded PNG container validation, without inflating IDAT on the PTY path.
//! Reuse the existing checksum/header validator, then enforce static-image
//! chunk order and the stricter graphics surface/allocation limits.
use super::{bounded_image_size, MAX_DECODED_IMAGE_BYTES, MAX_IMAGE_BYTES};

pub(super) fn png_size(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() > MAX_IMAGE_BYTES || data.len() < 33 {
        return None;
    }
    // Reject allocation bombs before the checksum pass over the whole payload.
    let width = u32::from_be_bytes(data.get(16..20)?.try_into().ok()?);
    let height = u32::from_be_bytes(data.get(20..24)?.try_into().ok()?);
    if !bounded_image_size(width, height) {
        return None;
    }
    let info = crate::clipboard_image::validate_png(data).ok()?;
    if !bounded_image_size(info.width, info.height) {
        return None;
    }
    // validate_png proved the complete 13-byte IHDR exists and its fields are
    // supported. Include 16-bit samples and filter bytes in the scanline bound.
    let channels = match data[25] {
        0 | 3 => 1,
        2 => 3,
        4 => 2,
        6 => 4,
        _ => return None,
    };
    let passes: &[(u32, u32, u32, u32)] = if data[28] == 0 {
        &[(0, 0, 1, 1)]
    } else {
        &[
            (0, 0, 8, 8),
            (4, 0, 8, 8),
            (0, 4, 4, 8),
            (2, 0, 4, 4),
            (0, 2, 2, 4),
            (1, 0, 2, 2),
            (0, 1, 1, 2),
        ]
    };
    let mut decoded = 0u64;
    for &(x, y, dx, dy) in passes {
        let width = info.width.saturating_sub(x).div_ceil(dx);
        let height = info.height.saturating_sub(y).div_ceil(dy);
        if width == 0 || height == 0 {
            continue;
        }
        let row = u64::from(width)
            .checked_mul(channels)?
            .checked_mul(u64::from(data[24]))?
            .div_ceil(8)
            .checked_add(1)?;
        decoded = decoded.checked_add(row.checked_mul(u64::from(height))?)?;
        if decoded > MAX_DECODED_IMAGE_BYTES as u64 {
            return None;
        }
    }
    let mut cursor = 8usize;
    let mut palette = false;
    let mut idat = false;
    let mut idat_closed = false;
    let mut idat_bytes = 0usize;
    while cursor < data.len() {
        let length = usize::try_from(u32::from_be_bytes(
            data.get(cursor..cursor.checked_add(4)?)?.try_into().ok()?,
        ))
        .ok()?;
        let start = cursor.checked_add(8)?;
        let end = start.checked_add(length)?;
        let kind = data.get(cursor.checked_add(4)?..start)?;
        if kind[2].is_ascii_lowercase() {
            return None;
        } // reserved bit
        match kind {
            b"IHDR" => {}
            b"PLTE" => {
                if palette
                    || idat
                    || length == 0
                    || length % 3 != 0
                    || length > 768
                    || matches!(data[25], 0 | 4)
                {
                    return None;
                }
                if data[25] == 3 && length / 3 > (1usize << data[24]) {
                    return None;
                }
                palette = true;
            }
            b"IDAT" => {
                if idat_closed || (data[25] == 3 && !palette) {
                    return None;
                }
                idat = true;
                idat_bytes = idat_bytes.checked_add(length)?;
            }
            b"IEND" => {
                if idat_bytes == 0 {
                    return None;
                }
            }
            // APNG would silently display only its base frame. Fail honestly.
            b"acTL" | b"fcTL" | b"fdAT" => return None,
            _ => {
                if kind[0].is_ascii_uppercase() {
                    return None;
                } // unknown critical chunk
                if idat {
                    idat_closed = true;
                }
            }
        }
        cursor = end.checked_add(4)?;
    }
    Some((info.width, info.height))
}
