//! Binary client/server wire protocol (M2). Length-prefixed bincode frames.
//! The client streams input + size; the server streams rendered frames.
//! See docs/08 §1.

use std::io::{self, Read, Write};

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyEvent, MouseEvent};
use ratatui::style::{Color, Modifier};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::sound::SoundSignal;
use crate::terminal::theme_probe::TerminalColors;

pub const PROTOCOL_VERSION: u32 = 6;
const MAX_FRAME: usize = 64 * 1024 * 1024;

#[derive(Serialize, Deserialize, Clone)]
pub enum ClientMessage {
    Hello {
        version: u32,
        cols: u16,
        rows: u16,
    },
    Key(KeyEvent),
    /// A key whose host reported a Shift-translated character while the client
    /// could still recover the layout-aware unshifted identity for CSI-u.
    KeyWithIdentity {
        key: KeyEvent,
        unshifted: char,
    },
    Mouse(MouseEvent),
    Paste(String),
    Resize {
        cols: u16,
        rows: u16,
    },
    Detach,
    /// Response to [`ServerMessage::Ready`] when terminal colors were requested.
    TerminalColors(Option<TerminalColors>),
}

#[derive(Serialize, Deserialize, Clone)]
pub enum ServerMessage {
    Welcome {
        version: u32,
        error: Option<String>,
    },
    /// A full frame — sent first, on resize, and to a freshly-attached client.
    Frame(FrameData),
    /// Only the cells that changed since the last frame (docs/18 — the wire-level
    /// diff that keeps remote attach over SSH cheap; also cuts local serialization).
    FrameDiff(FrameDiff),
    /// Ring the bell + raise a desktop notification on the client's terminal.
    Notify(String),
    /// Play the selected notification cue on the client.
    Sound(SoundSignal),
    /// Set the client's system clipboard (OSC 52) to this text — sent when a
    /// mouse selection finishes, so drag-to-select auto-copies.
    Clipboard(String),
    /// Open this URL in the client's browser (docs/58 — Ctrl+click a link in a
    /// pane). Client-side for the same reason the clipboard is: with `--remote`
    /// the server is on another machine, and the browser you want is the one in
    /// front of you.
    OpenUrl(String),
    /// Ask this display client to reconnect to another validated named session.
    /// It contains no socket path or command and is resolved by the client using
    /// the same local/remote session rules as process startup.
    SwitchSession {
        name: String,
    },
    /// Tell the client to detach (server keeps running).
    Detach,
    ServerShutdown {
        reason: String,
    },
    /// Completes negotiation after `Welcome`. Kept separate so a new client can
    /// still decode the version-mismatch reply from an older server.
    Ready {
        probe_terminal: bool,
    },
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct FrameData {
    pub width: u16,
    pub height: u16,
    /// Row-major, `width * height` cells.
    pub cells: Vec<CellData>,
    pub cursor: Option<(u16, u16)>,
    /// When `cursor` is Some, whether the host caret should be shown. Hidden
    /// in-view PTY still parks IME (Pi `?25l` after CUP to its input marker).
    #[serde(default)]
    pub cursor_visible: bool,
}

/// A sparse update sent instead of a full `FrameData` when the dimensions are
/// unchanged: a list of [`DiffRun`]s (contiguous, same-style changed cells) plus
/// the cursor. The run encoding shares one color/style across a stretch of text
/// (the classic run-based terminal-update technique), so a line of output costs
/// a couple of bytes per char, not the ~10 a per-cell diff would.
#[derive(Serialize, Deserialize, Clone)]
pub struct FrameDiff {
    pub width: u16,
    pub height: u16,
    pub runs: Vec<DiffRun>,
    pub cursor: Option<(u16, u16)>,
    #[serde(default)]
    pub cursor_visible: bool,
}

/// A maximal run of adjacent changed cells that share `fg`/`bg`/`mods`. `symbols`
/// holds one entry per cell (kept separate, not concatenated, so grapheme/wide
/// cells survive the round-trip).
#[derive(Serialize, Deserialize, Clone)]
pub struct DiffRun {
    pub start: u32,
    pub fg: u32,
    pub bg: u32,
    pub mods: u16,
    pub symbols: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct CellData {
    pub symbol: String,
    pub fg: u32,
    pub bg: u32,
    pub mods: u16,
}

/// Changed cells of `new` vs `old` (assumed same dimensions), coalesced into
/// same-style runs. The production path uses [`diff_buffer`] (which diffs a live
/// `Buffer` with no per-cell allocation); this `FrameData`-to-`FrameData` form
/// remains for round-trip tests of the diff/[`apply_diff`] wire encoding.
#[cfg(test)]
pub fn diff_runs(old: &FrameData, new: &FrameData) -> Vec<DiffRun> {
    let mut runs: Vec<DiffRun> = Vec::new();
    for (i, (a, b)) in old.cells.iter().zip(&new.cells).enumerate() {
        if a == b {
            continue;
        }
        let i = i as u32;
        // Extend the previous run if this cell is contiguous and same-style.
        if let Some(run) = runs.last_mut() {
            if run.start + run.symbols.len() as u32 == i
                && run.fg == b.fg
                && run.bg == b.bg
                && run.mods == b.mods
            {
                run.symbols.push(b.symbol.clone());
                continue;
            }
        }
        runs.push(DiffRun {
            start: i,
            fg: b.fg,
            bg: b.bg,
            mods: b.mods,
            symbols: vec![b.symbol.clone()],
        });
    }
    runs
}

/// Apply a `FrameDiff` to a full `FrameData` in place. The client no longer
/// reconstructs full frames (it writes changed cells straight to the terminal), so
/// this is now only a test helper for the diff/round-trip checks.
#[cfg(test)]
pub fn apply_diff(frame: &mut FrameData, diff: &FrameDiff) {
    frame.width = diff.width;
    frame.height = diff.height;
    frame.cursor = diff.cursor;
    frame.cursor_visible = diff.cursor_visible;
    for run in &diff.runs {
        for (k, sym) in run.symbols.iter().enumerate() {
            if let Some(slot) = frame.cells.get_mut(run.start as usize + k) {
                slot.symbol = sym.clone();
                slot.fg = run.fg;
                slot.bg = run.bg;
                slot.mods = run.mods;
            }
        }
    }
}

// ── framing ─────────────────────────────────────────────────────────────────

pub fn write_message<W: Write>(w: &mut W, msg: &impl Serialize) -> io::Result<()> {
    write_message_counted(w, msg).map(|_| ())
}

/// Write one framed message and return its exact wire length, including the
/// four-byte size prefix. This lets diagnostics count bytes without encoding a
/// rendered frame twice.
pub fn write_message_counted<W: Write>(w: &mut W, msg: &impl Serialize) -> io::Result<usize> {
    let bytes = bincode::serde::encode_to_vec(msg, bincode::config::standard())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    w.write_all(&(bytes.len() as u32).to_le_bytes())?;
    w.write_all(&bytes)?;
    w.flush()?;
    Ok(bytes.len().saturating_add(4))
}

pub fn read_message<R: Read, M: DeserializeOwned>(r: &mut R) -> io::Result<M> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    let (msg, _) = bincode::serde::decode_from_slice(&buf, bincode::config::standard())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(msg)
}

// ── buffer ↔ frame ──────────────────────────────────────────────────────────

/// Is `sym` a double-width glyph (emoji / CJK)? Fast-paths the overwhelmingly
/// common single-byte ASCII cell so the per-cell serialization loop pays the
/// unicode-width lookup only for the rare multi-byte cell.
#[inline]
fn is_wide(sym: &str) -> bool {
    sym.len() > 1 && unicode_width::UnicodeWidthStr::width(sym) == 2
}

/// The symbol to put on the wire for a cell, given whether its left neighbour was
/// a wide glyph. A wide glyph spans two columns; the second is a **continuation**
/// that must print nothing (an empty symbol), because the client advances its
/// cursor one column per cell while the terminal advanced two for the glyph.
/// ratatui leaves that column as a space (`symbol: None`) plus an internal `skip`
/// flag we don't serialize, so without this the space blits over the glyph's
/// right half and shifts the row — the emoji glitch, here fixed for *every*
/// rendered surface (panes, modals, sidebars, the git tab, …), not just panes.
#[inline]
fn wire_symbol(raw: &str, prev_wide: bool) -> &str {
    if prev_wide {
        ""
    } else {
        raw
    }
}

pub fn frame_from_buffer(
    buf: &Buffer,
    cursor: Option<(u16, u16)>,
    cursor_visible: bool,
) -> FrameData {
    let area = buf.area;
    let mut cells = Vec::with_capacity(area.width as usize * area.height as usize);
    for y in 0..area.height {
        let mut prev_wide = false;
        for x in 0..area.width {
            let c = &buf[(area.x + x, area.y + y)];
            let raw = c.symbol();
            cells.push(CellData {
                symbol: wire_symbol(raw, prev_wide).to_string(),
                fg: pack(c.fg),
                bg: pack(c.bg),
                mods: c.modifier.bits(),
            });
            prev_wide = is_wide(raw);
        }
    }
    FrameData {
        width: area.width,
        height: area.height,
        cells,
        cursor,
        cursor_visible: cursor.is_some() && cursor_visible,
    }
}

/// Diff the live ratatui `buf` against `prev` (the last sent frame) **in place**:
/// returns the changed-cell runs *and* updates `prev`'s cells to match. A frame then
/// costs one O(screen) comparison and allocates only for cells that actually changed
/// — not a full `Buffer::clone()` + a per-cell `String` via [`frame_from_buffer`]
/// every frame, which dominated server CPU (and so input latency) on large terminals
/// and at 60 fps. Assumes `buf` and `prev` share dimensions (the caller sends a full
/// frame on resize). Mirrors [`diff_runs`] but reads cells straight from the buffer.
pub fn diff_buffer(prev: &mut FrameData, buf: &Buffer) -> Vec<DiffRun> {
    let area = buf.area;
    let width = prev.width as u32;
    let mut runs: Vec<DiffRun> = Vec::new();
    for y in 0..area.height {
        let mut prev_wide = false;
        for x in 0..area.width {
            let c = &buf[(area.x + x, area.y + y)];
            let raw = c.symbol();
            // Blank a wide glyph's continuation column (see `wire_symbol`). Computed
            // before the bounds check so the wide-run accounting can't desync.
            let sym = wire_symbol(raw, prev_wide);
            prev_wide = is_wide(raw);
            let i = y as u32 * width + x as u32;
            let Some(slot) = prev.cells.get_mut(i as usize) else {
                continue;
            };
            let (fg, bg, mods) = (pack(c.fg), pack(c.bg), c.modifier.bits());
            if slot.fg == fg && slot.bg == bg && slot.mods == mods && slot.symbol == sym {
                continue; // unchanged — no allocation, no work
            }
            // Extend the previous run if contiguous + same style, else start one.
            match runs.last_mut() {
                Some(run)
                    if run.start + run.symbols.len() as u32 == i
                        && run.fg == fg
                        && run.bg == bg
                        && run.mods == mods =>
                {
                    run.symbols.push(sym.to_string());
                }
                _ => runs.push(DiffRun {
                    start: i,
                    fg,
                    bg,
                    mods,
                    symbols: vec![sym.to_string()],
                }),
            }
            // Update `prev` in place, reusing the existing String's allocation.
            slot.fg = fg;
            slot.bg = bg;
            slot.mods = mods;
            slot.symbol.clear();
            slot.symbol.push_str(sym);
        }
    }
    runs
}

pub fn pack(c: Color) -> u32 {
    let indexed = |i: u8| (1 << 24) | i as u32;
    match c {
        Color::Reset => 0,
        Color::Indexed(i) => indexed(i),
        Color::Rgb(r, g, b) => (2 << 24) | ((r as u32) << 16) | ((g as u32) << 8) | b as u32,
        Color::Black => indexed(0),
        Color::Red => indexed(1),
        Color::Green => indexed(2),
        Color::Yellow => indexed(3),
        Color::Blue => indexed(4),
        Color::Magenta => indexed(5),
        Color::Cyan => indexed(6),
        Color::Gray => indexed(7),
        Color::DarkGray => indexed(8),
        Color::LightRed => indexed(9),
        Color::LightGreen => indexed(10),
        Color::LightYellow => indexed(11),
        Color::LightBlue => indexed(12),
        Color::LightMagenta => indexed(13),
        Color::LightCyan => indexed(14),
        Color::White => indexed(15),
    }
}

pub fn unpack(u: u32) -> Color {
    match u >> 24 {
        1 => Color::Indexed((u & 0xff) as u8),
        2 => Color::Rgb(
            ((u >> 16) & 0xff) as u8,
            ((u >> 8) & 0xff) as u8,
            (u & 0xff) as u8,
        ),
        _ => Color::Reset,
    }
}

pub fn unpack_mods(bits: u16) -> Modifier {
    Modifier::from_bits_truncate(bits)
}

/// Whether the current terminal advertises 24-bit color support.
pub fn truecolor_supported() -> bool {
    std::env::var("COLORTERM")
        .map(|v| v.contains("truecolor") || v.contains("24bit"))
        .unwrap_or(false)
}

/// Downsample an RGB color to the nearest xterm-256 index. Terminals without
/// truecolor (e.g. macOS Terminal.app) garble `38;2;r;g;b`, so we fall back to
/// `38;5;n` which every 256-color terminal renders correctly.
pub fn to_256(c: Color) -> Color {
    match c {
        Color::Rgb(r, g, b) => Color::Indexed(nearest_256(r, g, b)),
        other => other,
    }
}

fn nearest_256(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [i32; 6] = [0, 95, 135, 175, 215, 255];
    let cube = |v: u8| -> (usize, i32) {
        let mut best = 0;
        let mut bd = i32::MAX;
        for (i, &l) in LEVELS.iter().enumerate() {
            let d = (v as i32 - l).abs();
            if d < bd {
                bd = d;
                best = i;
            }
        }
        (best, LEVELS[best])
    };
    let (ri, rv) = cube(r);
    let (gi, gv) = cube(g);
    let (bi, bv) = cube(b);
    let cube_idx = (16 + 36 * ri + 6 * gi + bi) as u8;
    let cube_dist = sq(r as i32 - rv) + sq(g as i32 - gv) + sq(b as i32 - bv);

    let gray_avg = (r as i32 + g as i32 + b as i32) / 3;
    let gi2 = ((gray_avg - 8).max(0) / 10).min(23);
    let gray_val = 8 + 10 * gi2;
    let gray_dist = sq(r as i32 - gray_val) + sq(g as i32 - gray_val) + sq(b as i32 - gray_val);

    if gray_dist < cube_dist {
        (232 + gi2) as u8
    } else {
        cube_idx
    }
}

fn sq(x: i32) -> i32 {
    x * x
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_roundtrip() {
        let msg = ServerMessage::Frame(FrameData {
            width: 2,
            height: 1,
            cells: vec![
                CellData {
                    symbol: "x".into(),
                    fg: pack(Color::Rgb(1, 2, 3)),
                    bg: 0,
                    mods: 0,
                },
                CellData {
                    symbol: "y".into(),
                    fg: pack(Color::Indexed(5)),
                    bg: 0,
                    mods: 0,
                },
            ],
            cursor: Some((1, 0)),
            cursor_visible: true,
        });
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();
        let back: ServerMessage = read_message(&mut &buf[..]).unwrap();
        match back {
            ServerMessage::Frame(f) => {
                assert_eq!(f.width, 2);
                assert_eq!(f.cells[0].symbol, "x");
                assert_eq!(unpack(f.cells[0].fg), Color::Rgb(1, 2, 3));
                assert_eq!(unpack(f.cells[1].fg), Color::Indexed(5));
                assert_eq!(f.cursor, Some((1, 0)));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn key_identity_roundtrips_for_remote_csi_u_encoding() {
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};

        let message = ClientMessage::KeyWithIdentity {
            key: KeyEvent::new(KeyCode::Char('?'), KeyModifiers::ALT | KeyModifiers::SHIFT),
            unshifted: '/',
        };
        let mut bytes = Vec::new();
        write_message(&mut bytes, &message).unwrap();
        assert!(matches!(
            read_message::<_, ClientMessage>(&mut &bytes[..]).unwrap(),
            ClientMessage::KeyWithIdentity { key, unshifted }
                if key.code == KeyCode::Char('?')
                    && key.modifiers == KeyModifiers::ALT | KeyModifiers::SHIFT
                    && unshifted == '/'
        ));
    }

    #[test]
    fn sound_signal_roundtrip_preserves_style_and_cue() {
        let signal = crate::sound::SoundSignal {
            cue: crate::sound::SoundCue::Blocked,
            style: crate::sound::SoundStyle::Pulse,
        };
        let mut bytes = Vec::new();
        write_message(&mut bytes, &ServerMessage::Sound(signal)).unwrap();
        assert!(matches!(
            read_message::<_, ServerMessage>(&mut &bytes[..]).unwrap(),
            ServerMessage::Sound(decoded) if decoded == signal
        ));
    }

    #[test]
    fn logical_session_switch_roundtrips_without_a_socket_path() {
        let mut bytes = Vec::new();
        write_message(
            &mut bytes,
            &ServerMessage::SwitchSession { name: "api".into() },
        )
        .unwrap();
        assert!(matches!(
            read_message::<_, ServerMessage>(&mut &bytes[..]).unwrap(),
            ServerMessage::SwitchSession { name } if name == "api"
        ));
    }

    /// A wide glyph written through ratatui's own `set_string` (the path modals,
    /// the sidebar, the git tab, and every other chrome surface use) leaves its
    /// second column as a space (`symbol: None`). Serialization must blank that
    /// continuation so the client doesn't blit the space over the glyph's right
    /// half — the emoji-shift bug, previously fixed for panes only.
    #[test]
    fn wire_blanks_wide_glyph_continuation_from_ratatui_chrome() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::style::Style;
        let mut buf = Buffer::empty(Rect::new(0, 0, 6, 1));
        buf.set_string(0, 0, "🔴X", Style::default());
        // Sanity: ratatui itself leaves a *space* in the continuation column.
        assert_eq!(
            buf[(1, 0)].symbol(),
            " ",
            "ratatui leaves a space, not empty"
        );

        // Full-frame serialization blanks it.
        let frame = frame_from_buffer(&buf, None, false);
        assert_eq!(frame.cells[0].symbol, "🔴", "emoji at col 0");
        assert_eq!(
            frame.cells[1].symbol, "",
            "continuation blanked (was a space)"
        );
        assert_eq!(frame.cells[2].symbol, "X", "next glyph stays at col 2");

        // The per-frame diff path blanks it too, so a modal that repaints via
        // `diff_buffer` (the hot path) is fixed as well.
        let mut prev = frame_from_buffer(&Buffer::empty(Rect::new(0, 0, 6, 1)), None, false);
        let runs = diff_buffer(&mut prev, &buf);
        let changed: Vec<&str> = runs
            .iter()
            .flat_map(|r| r.symbols.iter().map(String::as_str))
            .collect();
        assert!(
            changed.contains(&"🔴") && changed.contains(&"") && !changed.contains(&" "),
            "diff sends the emoji + an empty continuation, never a space: {changed:?}"
        );
    }

    #[test]
    fn diff_buffer_emits_changed_runs_and_updates_prev_in_place() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        let area = Rect::new(0, 0, 4, 2);
        // `prev` starts as the blank buffer.
        let mut prev = frame_from_buffer(&Buffer::empty(area), None, false);
        // A buffer with two adjacent changed cells on row 0.
        let mut buf = Buffer::empty(area);
        buf[(1, 0)].set_symbol("a");
        buf[(2, 0)].set_symbol("b");

        let runs = diff_buffer(&mut prev, &buf);
        assert_eq!(runs.len(), 1, "adjacent same-style cells coalesce");
        assert_eq!(runs[0].start, 1);
        assert_eq!(runs[0].symbols, vec!["a".to_string(), "b".to_string()]);
        // `prev` was updated in place to equal the buffer.
        assert_eq!(prev.cells, frame_from_buffer(&buf, None, false).cells);
        // Diffing the same buffer again yields nothing (no spurious frames).
        assert!(diff_buffer(&mut prev, &buf).is_empty());
    }

    #[test]
    fn diff_and_apply_roundtrip() {
        let cell = |s: &str| CellData {
            symbol: s.into(),
            fg: 0,
            bg: 0,
            mods: 0,
        };
        let old = FrameData {
            width: 3,
            height: 1,
            cells: vec![cell("a"), cell("b"), cell("c")],
            cursor: Some((0, 0)),
            cursor_visible: true,
        };
        let mut new = old.clone();
        new.cells[1] = cell("X"); // one cell changed
        new.cursor = Some((1, 0));

        let runs = diff_runs(&old, &new);
        assert_eq!(runs.len(), 1, "one run for the single changed cell");
        assert_eq!(runs[0].start, 1);
        assert_eq!(runs[0].symbols, vec!["X".to_string()]);

        // Applying the diff to `old` reconstructs `new` exactly.
        let mut rebuilt = old.clone();
        apply_diff(
            &mut rebuilt,
            &FrameDiff {
                width: new.width,
                height: new.height,
                runs,
                cursor: new.cursor,
                cursor_visible: new.cursor_visible,
            },
        );
        assert!(rebuilt == new, "diff reconstructs the new frame");
    }

    #[test]
    fn diff_coalesces_adjacent_same_style_into_one_run() {
        let c = |s: &str, fg: u32| CellData {
            symbol: s.into(),
            fg,
            bg: 0,
            mods: 0,
        };
        let old = FrameData {
            width: 5,
            height: 1,
            cells: vec![c(" ", 0), c(" ", 0), c(" ", 0), c(" ", 0), c(" ", 0)],
            cursor: None,
            cursor_visible: false,
        };
        let mut new = old.clone();
        // "hi" same color at 1..3, then a different-color "X" at 3.
        new.cells[1] = c("h", 7);
        new.cells[2] = c("i", 7);
        new.cells[3] = c("X", 9);
        let runs = diff_runs(&old, &new);
        assert_eq!(
            runs.len(),
            2,
            "same-style cells coalesce; a new style breaks"
        );
        assert_eq!(runs[0].start, 1);
        assert_eq!(runs[0].symbols, vec!["h".to_string(), "i".to_string()]);
        assert_eq!(runs[1].symbols, vec!["X".to_string()]);
    }

    #[test]
    fn client_reconstructs_a_frame_then_diff_stream_over_the_wire() {
        // Models exactly what a client receives: a full Frame, then a FrameDiff —
        // both serialized + deserialized, then applied like the client does.
        let cell = |s: &str, fg: u32| CellData {
            symbol: s.into(),
            fg,
            bg: 0,
            mods: 0,
        };
        let f1 = FrameData {
            width: 2,
            height: 2,
            cells: vec![cell("a", 1), cell("b", 2), cell("c", 3), cell("d", 4)],
            cursor: Some((0, 0)),
            cursor_visible: true,
        };
        let mut f2 = f1.clone();
        f2.cells[3] = cell("Z", 9);
        f2.cursor = Some((1, 1));

        // Server side: full frame, then a diff.
        let frame_msg = ServerMessage::Frame(f1.clone());
        let diff_msg = ServerMessage::FrameDiff(FrameDiff {
            width: f2.width,
            height: f2.height,
            runs: diff_runs(&f1, &f2),
            cursor: f2.cursor,
            cursor_visible: f2.cursor_visible,
        });
        let mut wire = Vec::new();
        write_message(&mut wire, &frame_msg).unwrap();
        write_message(&mut wire, &diff_msg).unwrap();

        // Client side: read both, reconstruct.
        let mut r = &wire[..];
        let mut current = match read_message::<_, ServerMessage>(&mut r).unwrap() {
            ServerMessage::Frame(f) => f,
            _ => panic!("first message must be a full Frame"),
        };
        if let ServerMessage::FrameDiff(d) = read_message::<_, ServerMessage>(&mut r).unwrap() {
            apply_diff(&mut current, &d);
        } else {
            panic!("expected a FrameDiff");
        }
        assert!(current == f2, "client reconstructs f2 from Frame + Diff");
    }

    #[test]
    fn downsample_to_256() {
        // near-black → a dark index (grayscale or cube corner), always Indexed.
        match to_256(Color::Rgb(0x11, 0x11, 0x16)) {
            Color::Indexed(i) => assert!(!(16..232).contains(&i), "got {i}"),
            other => panic!("expected indexed, got {other:?}"),
        }
        assert!(matches!(
            to_256(Color::Rgb(0xe2, 0xb0, 0x6a)),
            Color::Indexed(_)
        ));
        // Already-indexed and reset pass through unchanged.
        assert_eq!(to_256(Color::Indexed(5)), Color::Indexed(5));
        assert_eq!(to_256(Color::Reset), Color::Reset);
    }
}

#[cfg(all(test, feature = "dev-tools"))]
mod size_probe {
    use super::*;
    use ratatui::style::Color;

    fn cell(s: &str, fg: Color) -> CellData {
        CellData {
            symbol: s.into(),
            fg: pack(fg),
            bg: pack(Color::Reset),
            mods: 0,
        }
    }
    fn full_frame(w: u16, h: u16) -> FrameData {
        let mut cells = Vec::new();
        for _ in 0..(w as usize * h as usize) {
            cells.push(cell(" ", Color::Reset));
        }
        FrameData {
            width: w,
            height: h,
            cells,
            cursor: Some((0, 0)),
            cursor_visible: true,
        }
    }
    fn ser(m: &ServerMessage) -> usize {
        let mut b = Vec::new();
        write_message(&mut b, m).unwrap();
        b.len()
    }

    /// Run with `cargo test --features dev-tools measure_wire_sizes -- --nocapture`.
    #[test]
    fn measure_wire_sizes() {
        let f0 = full_frame(120, 32);
        let full = ser(&ServerMessage::Frame(f0.clone()));
        // simulate typing one char
        let mut f1 = f0.clone();
        f1.cells[100] = cell("a", Color::Rgb(200, 255, 26));
        f1.cursor = Some((1, 0));
        let d1 = ser(&ServerMessage::FrameDiff(FrameDiff {
            width: 120,
            height: 32,
            runs: diff_runs(&f0, &f1),
            cursor: f1.cursor,
            cursor_visible: f1.cursor_visible,
        }));
        // simulate a line of output: 40 chars, same color
        let mut f2 = f0.clone();
        for i in 0..40 {
            f2.cells[200 + i] = cell("x", Color::Rgb(200, 255, 26));
        }
        let d2 = ser(&ServerMessage::FrameDiff(FrameDiff {
            width: 120,
            height: 32,
            runs: diff_runs(&f0, &f2),
            cursor: Some((40, 1)),
            cursor_visible: true,
        }));
        // a full-screen redraw of same-colored text (worst streaming case)
        let mut f3 = f0.clone();
        for c in f3.cells.iter_mut() {
            *c = cell("#", Color::Rgb(200, 255, 26));
        }
        let d3 = ser(&ServerMessage::FrameDiff(FrameDiff {
            width: 120,
            height: 32,
            runs: diff_runs(&f0, &f3),
            cursor: Some((0, 0)),
            cursor_visible: true,
        }));
        println!("FULL frame (120x32)      : {full} bytes");
        println!(
            "DIFF, 1 char typed       : {d1} bytes  ({}x smaller)",
            full / d1.max(1)
        );
        println!(
            "DIFF, 40-char line       : {d2} bytes  ({}x smaller)",
            full / d2.max(1)
        );
        println!(
            "DIFF, full-screen redraw : {d3} bytes  ({}x smaller)",
            full / d3.max(1)
        );
    }
}
