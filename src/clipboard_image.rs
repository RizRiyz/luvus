//! Bounded clipboard-image validation, PNG normalization, and private staging.
//!
//! Clipboard access belongs to the display client. This module contains only
//! platform-neutral data handling and server-side publication, so malformed
//! image data crosses neither the filesystem nor the app-loop boundary.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

pub(crate) const MAX_PNG_BYTES: usize = 48 * 1024 * 1024;
pub(crate) const MAX_DIMENSION: u32 = 8192;
pub(crate) const MAX_PIXELS: u64 = 12_000_000;

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const STAGED_PREFIX: &str = "luvus-image-";
const STAGED_SUFFIX: &str = ".png";
const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_STAGING_SCAN: usize = 1024;
const MAX_FRESH_STAGED: usize = 128;
#[cfg(any(windows, test))]
const STORED_BLOCK_BYTES: usize = u16::MAX as usize;
#[cfg(any(windows, test))]
const BI_RGB: u32 = 0;
#[cfg(any(windows, test))]
const BI_BITFIELDS: u32 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PngInfo {
    pub width: u32,
    pub height: u32,
}

/// An exact image-paste gesture. Alt is deliberately excluded so AltGr text
/// can never become a clipboard action, and repeats cannot stage duplicates.
pub(crate) fn is_image_paste_key(key: &KeyEvent) -> bool {
    key.kind == KeyEventKind::Press
        && matches!(key.code, KeyCode::Char('v' | 'V'))
        && (key.modifiers == KeyModifiers::CONTROL
            || key.modifiers == KeyModifiers::CONTROL | KeyModifiers::SHIFT)
}

/// Validate the PNG container and the decoded allocation implied by IHDR.
/// This intentionally does not decompress IDAT. Chunk CRCs and strict bounds
/// make the file safe to stage while the downstream agent remains responsible
/// for decoding its own input.
pub(crate) fn validate_png(bytes: &[u8]) -> Result<PngInfo, &'static str> {
    if bytes.len() > MAX_PNG_BYTES {
        return Err("PNG exceeds clipboard image limit");
    }
    if !bytes.starts_with(PNG_SIGNATURE) {
        return Err("invalid PNG signature");
    }

    let mut cursor = PNG_SIGNATURE.len();
    let mut info = None;
    let mut saw_idat = false;
    let mut saw_iend = false;
    while cursor < bytes.len() {
        let header_end = cursor.checked_add(8).ok_or("PNG chunk overflow")?;
        if header_end > bytes.len() {
            return Err("truncated PNG chunk header");
        }
        let length = u32::from_be_bytes(
            bytes[cursor..cursor + 4]
                .try_into()
                .map_err(|_| "invalid PNG chunk length")?,
        ) as usize;
        let kind = &bytes[cursor + 4..cursor + 8];
        if !kind.iter().all(|byte| byte.is_ascii_alphabetic()) {
            return Err("invalid PNG chunk type");
        }
        let data_start = cursor + 8;
        let data_end = data_start.checked_add(length).ok_or("PNG chunk overflow")?;
        let chunk_end = data_end.checked_add(4).ok_or("PNG chunk overflow")?;
        if chunk_end > bytes.len() {
            return Err("truncated PNG chunk");
        }
        let expected_crc = u32::from_be_bytes(
            bytes[data_end..chunk_end]
                .try_into()
                .map_err(|_| "invalid PNG checksum")?,
        );
        if crc32(&bytes[cursor + 4..data_end]) != expected_crc {
            return Err("invalid PNG checksum");
        }

        match kind {
            b"IHDR" => {
                if info.is_some() || cursor != PNG_SIGNATURE.len() || length != 13 {
                    return Err("invalid PNG header");
                }
                let width = read_be_u32(bytes, data_start)?;
                let height = read_be_u32(bytes, data_start + 4)?;
                let bit_depth = bytes[data_start + 8];
                let color_type = bytes[data_start + 9];
                if width == 0
                    || height == 0
                    || width > MAX_DIMENSION
                    || height > MAX_DIMENSION
                    || u64::from(width) * u64::from(height) > MAX_PIXELS
                {
                    return Err("PNG dimensions exceed clipboard image limit");
                }
                let channels = match (color_type, bit_depth) {
                    (0, 1 | 2 | 4 | 8 | 16) => 1u64,
                    (2, 8 | 16) => 3,
                    (3, 1 | 2 | 4 | 8) => 1,
                    (4, 8 | 16) => 2,
                    (6, 8 | 16) => 4,
                    _ => return Err("unsupported PNG pixel format"),
                };
                let row_bits = u64::from(width)
                    .checked_mul(channels)
                    .and_then(|value| value.checked_mul(u64::from(bit_depth)))
                    .ok_or("PNG dimensions overflow")?;
                let decoded = row_bits
                    .div_ceil(8)
                    .checked_add(1)
                    .and_then(|row| row.checked_mul(u64::from(height)))
                    .ok_or("PNG dimensions overflow")?;
                if decoded > MAX_PNG_BYTES as u64 {
                    return Err("decoded PNG exceeds clipboard image limit");
                }
                if bytes[data_start + 10] != 0
                    || bytes[data_start + 11] != 0
                    || bytes[data_start + 12] > 1
                {
                    return Err("unsupported PNG encoding");
                }
                info = Some(PngInfo { width, height });
            }
            b"IDAT" => {
                if info.is_none() || saw_iend {
                    return Err("PNG image data is out of order");
                }
                saw_idat = true;
            }
            b"IEND" => {
                if length != 0 || info.is_none() || !saw_idat || saw_iend {
                    return Err("invalid PNG end marker");
                }
                saw_iend = true;
                if chunk_end != bytes.len() {
                    return Err("trailing PNG data");
                }
            }
            _ if info.is_none() || saw_iend => return Err("PNG chunk is out of order"),
            _ => {}
        }
        cursor = chunk_end;
    }

    if !saw_iend {
        return Err("PNG is missing its end marker");
    }
    info.ok_or("PNG is missing its header")
}

/// Encode RGBA pixels without a compression dependency or a second full image
/// buffer. Stored DEFLATE blocks are larger than compressed output but keep the
/// transient memory bound predictable on the explicit paste path.
#[cfg(any(windows, test))]
pub(crate) fn encode_rgba_png(
    width: u32,
    height: u32,
    mut pixel: impl FnMut(u32, u32) -> [u8; 4],
) -> Result<Vec<u8>, &'static str> {
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or("image dimensions overflow")?;
    if width == 0
        || height == 0
        || width > MAX_DIMENSION
        || height > MAX_DIMENSION
        || pixels > MAX_PIXELS
    {
        return Err("image dimensions exceed clipboard image limit");
    }
    let raw_len = u64::from(height)
        .checked_mul(
            u64::from(width)
                .checked_mul(4)
                .and_then(|row| row.checked_add(1))
                .ok_or("image dimensions overflow")?,
        )
        .ok_or("image dimensions overflow")?;
    let block_count = raw_len.div_ceil(STORED_BLOCK_BYTES as u64);
    let estimated = raw_len
        .checked_add(block_count * 5)
        .and_then(|value| value.checked_add(128))
        .ok_or("PNG size overflow")?;
    if estimated > MAX_PNG_BYTES as u64 {
        return Err("encoded PNG exceeds clipboard image limit");
    }

    let mut output = Vec::with_capacity(estimated as usize);
    output.extend_from_slice(PNG_SIGNATURE);
    let mut ihdr = [0u8; 13];
    ihdr[..4].copy_from_slice(&width.to_be_bytes());
    ihdr[4..8].copy_from_slice(&height.to_be_bytes());
    ihdr[8] = 8;
    ihdr[9] = 6;
    append_chunk(&mut output, b"IHDR", &ihdr)?;

    let length_at = output.len();
    output.extend_from_slice(&[0; 4]);
    output.extend_from_slice(b"IDAT");
    let data_at = output.len();
    {
        let mut zlib = StoredZlib::new(&mut output);
        for y in 0..height {
            zlib.push(0);
            for x in 0..width {
                for channel in pixel(x, y) {
                    zlib.push(channel);
                }
            }
        }
        zlib.finish();
    }
    let idat_len = output.len() - data_at;
    let idat_len_u32 = u32::try_from(idat_len).map_err(|_| "PNG chunk exceeds limit")?;
    output[length_at..length_at + 4].copy_from_slice(&idat_len_u32.to_be_bytes());
    let checksum = crc32(&output[length_at + 4..]);
    output.extend_from_slice(&checksum.to_be_bytes());
    append_chunk(&mut output, b"IEND", &[])?;
    if output.len() > MAX_PNG_BYTES {
        return Err("encoded PNG exceeds clipboard image limit");
    }
    validate_png(&output)?;
    Ok(output)
}

/// Convert a bounded Windows DIB into the narrow RGBA PNG representation used
/// by Luvus. Clipboard access remains in the Windows adapter; keeping this byte
/// parser platform-neutral lets malformed Windows fixtures run in every CI job.
#[cfg(any(windows, test))]
pub(crate) fn dib_to_png(bytes: &[u8]) -> Result<Vec<u8>, &'static str> {
    let header_size = read_le_u32(bytes, 0)? as usize;
    if !matches!(header_size, 40 | 52 | 56 | 108 | 124) || header_size > bytes.len() {
        return Err("unsupported DIB header");
    }
    let width = read_le_i32(bytes, 4)?;
    let signed_height = read_le_i32(bytes, 8)?;
    if width <= 0 || signed_height == 0 || signed_height == i32::MIN {
        return Err("invalid DIB dimensions");
    }
    let width = width as u32;
    let height = signed_height.unsigned_abs();
    let planes = read_le_u16(bytes, 12)?;
    let bits = read_le_u16(bytes, 14)?;
    let compression = read_le_u32(bytes, 16)?;
    let colors_used = read_le_u32(bytes, 32)?;
    if planes != 1 || colors_used != 0 {
        return Err("unsupported DIB planes or palette");
    }
    if !matches!(
        (bits, compression),
        (24, BI_RGB) | (32, BI_RGB) | (32, BI_BITFIELDS)
    ) {
        return Err("unsupported DIB pixel format");
    }
    if header_size >= 124 && (read_le_u32(bytes, 112)? != 0 || read_le_u32(bytes, 116)? != 0) {
        return Err("embedded DIB profiles are unsupported");
    }

    let (red_mask, green_mask, blue_mask, alpha_mask, pixel_offset) = if compression == BI_BITFIELDS
    {
        let masks_at = if header_size >= 52 { 40 } else { header_size };
        let red = read_le_u32(bytes, masks_at)?;
        let green = read_le_u32(bytes, masks_at + 4)?;
        let blue = read_le_u32(bytes, masks_at + 8)?;
        let alpha = if header_size >= 56 {
            read_le_u32(bytes, masks_at + 12)?
        } else {
            0
        };
        validate_dib_masks(red, green, blue, alpha)?;
        let offset = if header_size >= 52 {
            header_size
        } else {
            header_size.checked_add(12).ok_or("DIB offset overflow")?
        };
        (red, green, blue, alpha, offset)
    } else {
        (0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0, header_size)
    };

    let row_bits = usize::try_from(width)
        .map_err(|_| "DIB width overflow")?
        .checked_mul(bits as usize)
        .ok_or("DIB row overflow")?;
    let row_stride = row_bits
        .checked_add(31)
        .map(|value| value / 32 * 4)
        .ok_or("DIB row overflow")?;
    let pixel_bytes = row_stride
        .checked_mul(height as usize)
        .ok_or("DIB storage overflow")?;
    let pixel_end = pixel_offset
        .checked_add(pixel_bytes)
        .ok_or("DIB storage overflow")?;
    if pixel_end > bytes.len() {
        return Err("truncated DIB pixels");
    }
    let top_down = signed_height < 0;
    encode_rgba_png(width, height, |x, y| {
        let source_y = if top_down { y } else { height - 1 - y };
        let row = pixel_offset + source_y as usize * row_stride;
        if bits == 24 {
            let offset = row + x as usize * 3;
            [bytes[offset + 2], bytes[offset + 1], bytes[offset], 255]
        } else {
            let offset = row + x as usize * 4;
            let value = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
            [
                dib_channel(value, red_mask),
                dib_channel(value, green_mask),
                dib_channel(value, blue_mask),
                if alpha_mask == 0 {
                    255
                } else {
                    dib_channel(value, alpha_mask)
                },
            ]
        }
    })
}

#[cfg(any(windows, test))]
fn validate_dib_masks(red: u32, green: u32, blue: u32, alpha: u32) -> Result<(), &'static str> {
    if red == 0
        || green == 0
        || blue == 0
        || !contiguous_mask(red)
        || !contiguous_mask(green)
        || !contiguous_mask(blue)
        || (alpha != 0 && !contiguous_mask(alpha))
        || red & green != 0
        || red & blue != 0
        || green & blue != 0
        || alpha & (red | green | blue) != 0
    {
        return Err("invalid DIB channel masks");
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn contiguous_mask(mask: u32) -> bool {
    if mask == 0 {
        return false;
    }
    let shifted = mask >> mask.trailing_zeros();
    shifted & shifted.wrapping_add(1) == 0
}

#[cfg(any(windows, test))]
fn dib_channel(value: u32, mask: u32) -> u8 {
    let shift = mask.trailing_zeros();
    let maximum = mask >> shift;
    let component = (value & mask) >> shift;
    ((u64::from(component) * 255 + u64::from(maximum) / 2) / u64::from(maximum)) as u8
}

#[cfg(any(windows, test))]
fn read_le_u16(bytes: &[u8], offset: usize) -> Result<u16, &'static str> {
    let end = offset.checked_add(2).ok_or("DIB integer overflow")?;
    Ok(u16::from_le_bytes(
        bytes
            .get(offset..end)
            .ok_or("truncated DIB integer")?
            .try_into()
            .map_err(|_| "invalid DIB integer")?,
    ))
}

#[cfg(any(windows, test))]
fn read_le_u32(bytes: &[u8], offset: usize) -> Result<u32, &'static str> {
    let end = offset.checked_add(4).ok_or("DIB integer overflow")?;
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..end)
            .ok_or("truncated DIB integer")?
            .try_into()
            .map_err(|_| "invalid DIB integer")?,
    ))
}

#[cfg(any(windows, test))]
fn read_le_i32(bytes: &[u8], offset: usize) -> Result<i32, &'static str> {
    Ok(read_le_u32(bytes, offset)? as i32)
}

/// Publish a validated image beneath the selected server session. The caller
/// runs on a client receiver or local input thread, never on the app loop.
pub(crate) fn stage_png(bytes: &[u8]) -> io::Result<PathBuf> {
    validate_png(bytes).map_err(|message| io::Error::new(io::ErrorKind::InvalidData, message))?;
    let dir = crate::persist::ensure_clipboard_image_dir()?;
    cleanup_staged(&dir);

    for _ in 0..8 {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
        let mut name = String::with_capacity(STAGED_PREFIX.len() + 32 + STAGED_SUFFIX.len());
        name.push_str(STAGED_PREFIX);
        for byte in random {
            use std::fmt::Write as _;
            let _ = write!(&mut name, "{byte:02x}");
        }
        name.push_str(STAGED_SUFFIX);
        let path = dir.join(name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut file) => {
                if let Err(error) = file.write_all(bytes) {
                    drop(file);
                    let _ = fs::remove_file(&path);
                    return Err(error);
                }
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique clipboard image path",
    ))
}

fn cleanup_staged(dir: &Path) {
    let now = SystemTime::now();
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut fresh = Vec::new();
    for entry in entries.take(MAX_STAGING_SCAN).flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with(STAGED_PREFIX) || !name.ends_with(STAGED_SUFFIX) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let stale = now
            .duration_since(modified)
            .is_ok_and(|age| age >= STALE_AFTER);
        if stale {
            let _ = fs::remove_file(path);
        } else {
            fresh.push((modified, path));
        }
    }
    if fresh.len() >= MAX_FRESH_STAGED {
        fresh.sort_by_key(|(modified, _)| *modified);
        let remove = fresh.len() - (MAX_FRESH_STAGED - 1);
        for (_, path) in fresh.into_iter().take(remove) {
            let _ = fs::remove_file(path);
        }
    }
}

fn read_be_u32(bytes: &[u8], offset: usize) -> Result<u32, &'static str> {
    let end = offset.checked_add(4).ok_or("PNG integer overflow")?;
    Ok(u32::from_be_bytes(
        bytes
            .get(offset..end)
            .ok_or("truncated PNG integer")?
            .try_into()
            .map_err(|_| "invalid PNG integer")?,
    ))
}

#[cfg(any(windows, test))]
fn append_chunk(output: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) -> Result<(), &'static str> {
    let length = u32::try_from(data.len()).map_err(|_| "PNG chunk exceeds limit")?;
    output.extend_from_slice(&length.to_be_bytes());
    let checksum_at = output.len();
    output.extend_from_slice(kind);
    output.extend_from_slice(data);
    let checksum = crc32(&output[checksum_at..]);
    output.extend_from_slice(&checksum.to_be_bytes());
    Ok(())
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

#[cfg(any(windows, test))]
struct StoredZlib<'a> {
    output: &'a mut Vec<u8>,
    block: Vec<u8>,
    adler_a: u32,
    adler_b: u32,
}

#[cfg(any(windows, test))]
impl<'a> StoredZlib<'a> {
    fn new(output: &'a mut Vec<u8>) -> Self {
        output.extend_from_slice(&[0x78, 0x01]);
        Self {
            output,
            block: Vec::with_capacity(STORED_BLOCK_BYTES),
            adler_a: 1,
            adler_b: 0,
        }
    }

    fn push(&mut self, byte: u8) {
        self.adler_a = (self.adler_a + u32::from(byte)) % 65_521;
        self.adler_b = (self.adler_b + self.adler_a) % 65_521;
        self.block.push(byte);
        if self.block.len() == STORED_BLOCK_BYTES {
            self.flush(false);
        }
    }

    fn flush(&mut self, final_block: bool) {
        self.output.push(u8::from(final_block));
        let length = self.block.len() as u16;
        self.output.extend_from_slice(&length.to_le_bytes());
        self.output.extend_from_slice(&(!length).to_le_bytes());
        self.output.extend_from_slice(&self.block);
        self.block.clear();
    }

    fn finish(mut self) {
        self.flush(true);
        let adler = (self.adler_b << 16) | self.adler_a;
        self.output.extend_from_slice(&adler.to_be_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored_rgba(png: &[u8]) -> Vec<u8> {
        let mut cursor = PNG_SIGNATURE.len();
        let mut zlib = None;
        while cursor < png.len() {
            let length = u32::from_be_bytes(png[cursor..cursor + 4].try_into().unwrap()) as usize;
            let data = cursor + 8..cursor + 8 + length;
            if &png[cursor + 4..cursor + 8] == b"IDAT" {
                zlib = Some(&png[data]);
                break;
            }
            cursor += 12 + length;
        }
        let zlib = zlib.expect("IDAT chunk");
        let mut cursor = 2;
        let mut raw = Vec::new();
        loop {
            let header = zlib[cursor];
            cursor += 1;
            assert_eq!(header & 0xfe, 0, "fixture uses stored DEFLATE blocks");
            let length = u16::from_le_bytes(zlib[cursor..cursor + 2].try_into().unwrap());
            let inverse = u16::from_le_bytes(zlib[cursor + 2..cursor + 4].try_into().unwrap());
            assert_eq!(length, !inverse);
            cursor += 4;
            raw.extend_from_slice(&zlib[cursor..cursor + length as usize]);
            cursor += length as usize;
            if header & 1 != 0 {
                break;
            }
        }
        assert_eq!(cursor + 4, zlib.len(), "four-byte Adler32 trailer");
        raw
    }

    #[test]
    fn exact_image_paste_chords_exclude_alt_and_repeats() {
        let key = |modifiers, kind| KeyEvent::new_with_kind(KeyCode::Char('v'), modifiers, kind);
        assert!(is_image_paste_key(&key(
            KeyModifiers::CONTROL,
            KeyEventKind::Press
        )));
        assert!(is_image_paste_key(&key(
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            KeyEventKind::Press
        )));
        assert!(!is_image_paste_key(&key(
            KeyModifiers::CONTROL | KeyModifiers::ALT,
            KeyEventKind::Press
        )));
        assert!(!is_image_paste_key(&key(
            KeyModifiers::CONTROL,
            KeyEventKind::Repeat
        )));
    }

    #[test]
    fn rgba_encoder_produces_a_bounded_structural_png() {
        let pixels = [
            [255, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
            [255, 255, 255, 128],
        ];
        let png = encode_rgba_png(2, 2, |x, y| pixels[(y * 2 + x) as usize]).unwrap();
        assert_eq!(
            validate_png(&png),
            Ok(PngInfo {
                width: 2,
                height: 2
            })
        );
        let mut corrupt = png;
        corrupt[20] ^= 1;
        assert_eq!(validate_png(&corrupt), Err("invalid PNG checksum"));
    }

    #[test]
    fn validator_rejects_dimensions_that_imply_excessive_decode_memory() {
        let png = encode_rgba_png(1, 1, |_, _| [0, 0, 0, 255]).unwrap();
        let mut oversized = png;
        oversized[16..20].copy_from_slice(&(MAX_DIMENSION + 1).to_be_bytes());
        let checksum = crc32(&oversized[12..29]);
        oversized[29..33].copy_from_slice(&checksum.to_be_bytes());
        assert_eq!(
            validate_png(&oversized),
            Err("PNG dimensions exceed clipboard image limit")
        );
    }

    #[test]
    fn bottom_up_24_bit_dib_preserves_rows_and_colors() {
        let mut dib = vec![0u8; 40 + 16];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&2i32.to_le_bytes());
        dib[8..12].copy_from_slice(&2i32.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&24u16.to_le_bytes());
        // DIB storage is bottom-up and each 24-bit row is padded to four bytes.
        dib[40..48].copy_from_slice(&[255, 0, 0, 255, 255, 255, 0, 0]);
        dib[48..56].copy_from_slice(&[0, 0, 255, 0, 255, 0, 0, 0]);

        let png = dib_to_png(&dib).unwrap();
        assert_eq!(
            validate_png(&png),
            Ok(PngInfo {
                width: 2,
                height: 2
            })
        );
        assert_eq!(
            stored_rgba(&png),
            [
                0, 255, 0, 0, 255, 0, 255, 0, 255, // top: red, green
                0, 0, 0, 255, 255, 255, 255, 255, 255, // bottom: blue, white
            ]
        );
    }

    #[test]
    fn top_down_v5_bitfields_preserve_alpha() {
        let mut dib = vec![0u8; 124 + 4];
        dib[0..4].copy_from_slice(&124u32.to_le_bytes());
        dib[4..8].copy_from_slice(&1i32.to_le_bytes());
        dib[8..12].copy_from_slice(&(-1i32).to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&32u16.to_le_bytes());
        dib[16..20].copy_from_slice(&BI_BITFIELDS.to_le_bytes());
        dib[40..44].copy_from_slice(&0x00ff_0000u32.to_le_bytes());
        dib[44..48].copy_from_slice(&0x0000_ff00u32.to_le_bytes());
        dib[48..52].copy_from_slice(&0x0000_00ffu32.to_le_bytes());
        dib[52..56].copy_from_slice(&0xff00_0000u32.to_le_bytes());
        dib[124..128].copy_from_slice(&0x8040_2010u32.to_le_bytes());

        let png = dib_to_png(&dib).unwrap();
        assert_eq!(
            validate_png(&png),
            Ok(PngInfo {
                width: 1,
                height: 1
            })
        );
        assert_eq!(stored_rgba(&png), [0, 64, 32, 16, 128]);
    }

    #[test]
    fn malformed_or_compressed_dib_fails_closed() {
        assert_eq!(dib_to_png(&[0; 12]), Err("unsupported DIB header"));
        let mut dib = vec![0u8; 44];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&1i32.to_le_bytes());
        dib[8..12].copy_from_slice(&1i32.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&32u16.to_le_bytes());
        dib[16..20].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(dib_to_png(&dib), Err("unsupported DIB pixel format"));
    }

    #[test]
    fn staging_uses_selected_private_session_storage() {
        let _env = crate::persist::test_env("clipboard-image-stage");
        crate::persist::ensure_session_dir();
        let png = encode_rgba_png(1, 1, |_, _| [1, 2, 3, 255]).unwrap();
        let path = stage_png(&png).unwrap();
        let expected = crate::persist::session_dir().join("clipboard-images");
        assert_eq!(path.parent(), Some(expected.as_path()));
        assert_eq!(fs::read(&path).unwrap(), png);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn staging_cleanup_is_bounded_and_preserves_unowned_files() {
        let _env = crate::persist::test_env("clipboard-image-cleanup");
        crate::persist::ensure_session_dir();
        let png = encode_rgba_png(1, 1, |_, _| [1, 2, 3, 255]).unwrap();
        let dir = crate::persist::ensure_clipboard_image_dir().unwrap();
        let unrelated = dir.join("keep.txt");
        fs::write(&unrelated, b"not owned by image staging").unwrap();

        for _ in 0..MAX_FRESH_STAGED + 3 {
            stage_png(&png).unwrap();
        }
        let owned = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(STAGED_PREFIX))
            })
            .count();
        assert_eq!(owned, MAX_FRESH_STAGED);
        assert_eq!(fs::read(unrelated).unwrap(), b"not owned by image staging");
    }
}
