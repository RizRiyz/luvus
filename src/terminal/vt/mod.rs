//! The terminal-emulator abstraction. The rest of luvus only ever talks to
//! `VtEngine`; the concrete implementation (`alacritty_terminal`) lives behind
//! it so it can be swapped (e.g. to `termwiz` for inline images) without
//! touching the app. See docs/05-pty-and-terminal.md.

pub mod alacritty;

use std::sync::{Arc, Mutex};

use ratatui::style::{Color, Modifier};

use crate::terminal::appearance::PaneAppearance;
use crate::terminal::pty::InputSender;

/// Internal continuation marker used by [`VtEngine::visible_rows_aligned`].
/// A terminal never renders NUL as text, so it can represent the second cell of
/// a wide glyph without being confused with an actual space between words.
pub(crate) const ALIGNED_WIDE_CELL: char = '\0';

const MAX_TERMINAL_HYPERLINK_URI_BYTES: usize = 4_096;
const MAX_TERMINAL_HYPERLINK_ID_BYTES: usize = 256;

/// One OSC 8 hyperlink retained by the terminal engine.
///
/// The URI is engine-neutral and its spans use visible grid coordinates. This
/// metadata is materialized only for deliberate text/link gestures, never for
/// ordinary rendering or agent detection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TerminalHyperlink {
    id: String,
    uri: String,
    spans: Vec<(u16, u16, u16)>,
}

impl TerminalHyperlink {
    pub(crate) fn uri(&self) -> &str {
        &self.uri
    }

    pub(crate) fn spans(&self) -> &[(u16, u16, u16)] {
        &self.spans
    }

    fn covers(&self, row: u16, col: u16) -> bool {
        self.spans
            .iter()
            .any(|(span_row, start, end)| *span_row == row && col >= *start && col < *end)
    }
}

/// Visible terminal text indexed one character per grid cell, plus the sparse
/// zero-width components attached to base cells. Keeping the latter separate
/// preserves column lookup without dropping combining marks, variation
/// selectors, or ZWJ emoji from copied text.
pub struct AlignedRows {
    rows: Vec<String>,
    zero_width: Vec<(u16, u16, Vec<char>)>,
    hyperlinks: Vec<TerminalHyperlink>,
}

impl AlignedRows {
    pub(crate) fn new(row_count: usize) -> Self {
        Self {
            rows: vec![String::new(); row_count],
            zero_width: Vec::new(),
            hyperlinks: Vec::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_rows(rows: Vec<String>) -> Self {
        Self {
            rows,
            zero_width: Vec::new(),
            hyperlinks: Vec::new(),
        }
    }

    pub(crate) fn push_cell(
        &mut self,
        row: u16,
        col: u16,
        character: char,
        zero_width: Option<&[char]>,
    ) {
        self.rows[row as usize].push(character);
        if let Some(chars) = zero_width.filter(|chars| !chars.is_empty()) {
            self.zero_width.push((row, col, chars.to_vec()));
        }
    }

    pub(crate) fn rows(&self) -> &[String] {
        &self.rows
    }

    pub(crate) fn push_hyperlink_cell(&mut self, row: u16, col: u16, id: &str, uri: &str) {
        if uri.is_empty()
            || uri.len() > MAX_TERMINAL_HYPERLINK_URI_BYTES
            || id.len() > MAX_TERMINAL_HYPERLINK_ID_BYTES
            || uri.chars().any(char::is_control)
            || id.chars().any(char::is_control)
        {
            return;
        }
        let hyperlink = match self
            .hyperlinks
            .iter_mut()
            .rev()
            .find(|hyperlink| hyperlink.id == id && hyperlink.uri == uri)
        {
            Some(hyperlink) => hyperlink,
            None => {
                self.hyperlinks.push(TerminalHyperlink {
                    id: id.to_string(),
                    uri: uri.to_string(),
                    spans: Vec::new(),
                });
                self.hyperlinks
                    .last_mut()
                    .expect("the terminal hyperlink was just inserted")
            }
        };
        match hyperlink.spans.last_mut() {
            Some((span_row, _, end)) if *span_row == row && *end == col => {
                *end = col.saturating_add(1)
            }
            _ => hyperlink.spans.push((row, col, col.saturating_add(1))),
        }
    }

    pub(crate) fn hyperlink_at(&self, row: u16, col: u16) -> Option<&TerminalHyperlink> {
        self.hyperlinks
            .iter()
            .find(|hyperlink| hyperlink.covers(row, col))
    }

    pub(crate) fn zero_width_at(&self, row: u16, col: u16) -> &[char] {
        self.zero_width
            .iter()
            .find(|(r, c, _)| *r == row && *c == col)
            .map_or(&[], |(_, _, chars)| chars)
    }
}

/// Which terminal engine backs a pane.
///
/// One variant today. It exists so that the choice of engine is a named
/// decision with one home, rather than a concrete type spelled out at each
/// construction site.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum VtEngineKind {
    #[default]
    Alacritty,
}

/// Build the engine backing one pane.
///
/// Every pane is constructed through here, so engine selection, and any
/// validation it later needs, live in one place while the rest of the
/// application keeps talking only to [`VtEngine`].
pub(crate) fn create_engine(
    kind: VtEngineKind,
    cols: u16,
    rows: u16,
    resp_tx: impl Into<InputSender>,
    history_budget_bytes: usize,
    appearance: PaneAppearance,
) -> Arc<Mutex<dyn VtEngine>> {
    let resp_tx = resp_tx.into();
    match kind {
        VtEngineKind::Alacritty => {
            Arc::new(Mutex::new(alacritty::AlacrittyEngine::with_appearance(
                cols,
                rows,
                resp_tx,
                history_budget_bytes,
                appearance,
            )))
        }
    }
}

/// A rendered cell's style, already mapped to ratatui colors/modifiers so the
/// trait surface stays free of engine-specific types. The cell's *symbol* (its
/// grapheme cluster) is passed alongside as a `&str`, not stored here, so the
/// common one-char case needs no per-cell allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderCell {
    pub fg: Color,
    pub bg: Color,
    pub mods: Modifier,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub x: u16,
    pub y: u16,
    pub visible: bool,
}

/// One owned terminal cell captured at a render boundary. Ordinary cells keep
/// only their scalar value; reusable suffix storage preserves the rare
/// combining, variation-selector, or joined-glyph case. The VT lock is released
/// before the UI projects this data into client buffers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DamageCell {
    pub column: u16,
    pub character: char,
    pub zero_width: Vec<char>,
    pub style: RenderCell,
}

/// A complete visible terminal row affected by the latest output generation.
///
/// Alacritty records narrower column bounds, but Luvus snapshots a complete
/// damaged row. This keeps wide-character bases, spacer cells, cleared tails,
/// and combining sequences correct while still avoiding work for every other
/// visible row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DamageRow {
    pub row: u16,
    pub cells: Vec<DamageCell>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DamageKind {
    Full,
    Partial,
}

/// Engine-neutral terminal damage captured under the VT lock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DamageSnapshot {
    pub generation: u64,
    pub kind: DamageKind,
    pub cursor: Cursor,
    pub composer_region: Option<CodexComposerRegion>,
    pub scroll_offset: usize,
    pub rows: Vec<DamageRow>,
}

/// Visible rows occupied by Codex's composer, including its blank padding rows.
/// Luvus uses this geometry only for the optional theme-aware composer frame;
/// the terminal engine remains responsible for recognizing the live grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodexComposerRegion {
    pub top: u16,
    pub bottom: u16,
}

/// Read-only scrollback accounting exposed by every terminal engine. Engines
/// that cannot enforce a native byte cap report a conservative estimate rather
/// than pretending it is exact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HistoryMetrics {
    pub offset: usize,
    pub retained_rows: usize,
    pub budget_bytes: usize,
    /// Legacy estimate retained for API compatibility. This is not process RSS.
    pub retained_bytes: usize,
    /// Estimated shallow allocation for the terminal engine's grids.
    pub estimated_grid_bytes: usize,
    /// Estimated shallow allocation held only for row reuse.
    pub cache_bytes: Option<usize>,
    /// Rows stored using the engine's compact cold-history representation.
    pub compacted_rows: Option<usize>,
    /// Physical cell slots allocated by the engine, excluding logical repeats.
    pub allocated_cells: Option<usize>,
    /// Cold-history blocks shared by packed rows.
    pub packed_blocks: Option<usize>,
    /// Shallow bytes owned by packed cold-history blocks.
    pub packed_bytes: Option<usize>,
    /// Rows backed by packed cold-history blocks.
    pub packed_rows: Option<usize>,
    /// Shallow bytes owned by ordinary dense row cell vectors.
    pub dense_row_bytes: Option<usize>,
    /// Bytes reserved by the outer row descriptor vectors.
    pub row_descriptor_bytes: Option<usize>,
    /// Approximate number of outer, row, and block allocations.
    pub allocation_count: Option<usize>,
    pub exact_bytes: bool,
}

/// Cell geometry for one retained terminal row.
///
/// Copy-mode navigation uses this instead of Unicode scalar counts. A terminal
/// cell is the only stable unit across narrow scripts, double-width glyphs,
/// combining marks, and emoji sequences.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedRowLayout {
    whitespace: Vec<bool>,
    has_text: bool,
}

impl RetainedRowLayout {
    pub(crate) fn new(whitespace: Vec<bool>, has_text: bool) -> Self {
        Self {
            whitespace,
            has_text,
        }
    }

    pub(crate) fn last_column(&self) -> usize {
        self.whitespace.len().saturating_sub(1)
    }

    pub(crate) fn is_whitespace(&self, column: usize) -> bool {
        self.whitespace.get(column).copied().unwrap_or(true)
    }

    pub(crate) fn has_text(&self) -> bool {
        self.has_text
    }
}

/// Minimal terminal-emulator surface. Owns the grid + scrollback.
pub trait VtEngine: Send {
    /// Feed child output. Must never panic on arbitrary bytes.
    fn advance(&mut self, bytes: &[u8]);

    /// Finish allocation maintenance deferred while parsing recent output.
    /// Unix calls this from its existing descriptor actor after a bounded
    /// activity window; Windows uses the app's coalesced output boundary.
    fn finish_output_batch(&mut self);

    /// Monotonic generation of successfully parsed terminal output.
    fn output_generation(&self) -> u64;

    /// Reflow to a new (cols, rows).
    fn resize(&mut self, cols: u16, rows: u16);

    /// Cursor position in the visible viewport.
    fn cursor(&self) -> Cursor;

    /// Detect Codex's live composer around the cursor. Returns `None` for
    /// scrollback, unrelated terminal content, or an incomplete layout.
    fn codex_composer_region(&self) -> Option<CodexComposerRegion>;

    /// Visit every visible cell as `(row, col, symbol, style)`. `symbol` is the
    /// cell's full grapheme cluster (base char + any combining/VS16/ZWJ chars),
    /// so emoji and accented text render whole. Wide-char spacer cells are
    /// skipped by the implementation.
    fn for_each_cell(&self, f: &mut dyn FnMut(u16, u16, &str, RenderCell));

    /// Capture owned visible rows affected since the last acknowledged render.
    /// Implementations may conservatively return [`DamageKind::Full`].
    /// Title changes must return Full until acknowledged: titles can also
    /// affect chrome outside terminal rows, including agent sidebar labels.
    fn damage_snapshot(&mut self) -> DamageSnapshot;

    /// Forget damage through `generation` only when no newer output exists.
    /// Returns `true` when the acknowledgement was accepted. A rejected
    /// acknowledgement must preserve all damage so a later frame can safely
    /// repeat work rather than lose output.
    fn acknowledge_damage(&mut self, generation: u64) -> bool;

    /// Return owned row and cell storage after the caller has finished using a
    /// snapshot. Implementations may retain a bounded pool or simply drop it.
    fn recycle_damage_snapshot(&mut self, snapshot: DamageSnapshot);

    /// Bottom `n` rows of the visible grid, for agent detection. Independent of
    /// the user's scroll position.
    fn detection_text(&self, n: u16) -> String;

    /// Bottom `n` non-empty live-screen rows. Agents with a tall blank footer
    /// use this without pulling scrollback into state detection.
    fn detection_text_non_empty(&self, n: u16) -> String {
        self.detection_text(n)
    }

    /// Every visible row as normalized plain text. Wide-character spacer cells
    /// are omitted, so callers must not use string indexes as terminal columns.
    fn visible_rows(&self) -> Vec<String>;

    /// Like [`Self::visible_rows`], but every terminal column contributes exactly
    /// one `char`. A wide glyph's continuation cell is represented by
    /// [`ALIGNED_WIDE_CELL`], so callers can preserve both cell coordinates and
    /// the distinction between a continuation and an actual space. Use this
    /// (never `visible_rows`) when a screen column must address text — e.g. the
    /// token under a double-click, or the link under a `Ctrl`-hover.
    fn visible_rows_aligned(&self) -> AlignedRows;

    /// Bounded public capture for harnesses. Implementations serialize only
    /// normalized grid text and SGR styles; raw child control sequences never
    /// cross this boundary.
    fn backend_capture(
        &self,
        mode: crate::terminal::backend::CaptureMode,
        lines: usize,
        ansi: bool,
        max_bytes: usize,
    ) -> crate::terminal::backend::CaptureResult;

    /// Latest window title set by the child via OSC 0/2, if any.
    fn title(&self) -> Option<String>;

    /// Scroll the viewport `delta` lines through scrollback: **positive scrolls
    /// up into history**, negative back toward the live bottom. Clamped to the
    /// retained history. No-op while on the alternate screen.
    /// Change this pane's retained-history memory budget. Lowering it drops
    /// excess history immediately. Engines without native byte accounting must
    /// use a conservative row cap and report estimated metrics.
    fn set_history_budget(&mut self, bytes: usize);

    fn scroll(&mut self, delta: i32);

    /// Jump the viewport to the very top of retained scrollback.
    fn scroll_to_top(&mut self);

    /// Snap the viewport back to the live bottom (offset 0).
    fn scroll_to_bottom(&mut self);

    /// How many lines the viewport is scrolled **above** the live bottom;
    /// `0` means it's live. Drives the scrollback indicator + cursor hiding.
    fn scroll_offset(&self) -> usize;

    /// Total lines of retained scrollback history (the maximum `scroll_offset`).
    /// Lets scroll mode jump to a proportional position (the `1`–`9` keys).
    fn history_len(&self) -> usize;

    /// Current scroll position and retained-history accounting.
    fn history_metrics(&self) -> HistoryMetrics;

    /// Number of rows available through [`Self::for_each_retained_row`], including
    /// the visible screen after the scrollback history.
    fn retained_row_count(&self) -> usize;

    /// Read one retained row by oldest-first index without materializing the
    /// entire history.
    #[cfg(test)]
    fn retained_row_text(&self, index: usize) -> Option<String>;

    /// Visit retained rows oldest-first using one reusable line buffer. The
    /// callback must not retain the borrowed text after it returns.
    fn for_each_retained_row(&self, f: &mut dyn FnMut(usize, &str));

    /// Extract an inclusive linear retained-row selection using terminal cell
    /// coordinates. Implementations must preserve complete wide glyphs and
    /// zero-width marks, join soft-wrapped display rows, preserve hard line
    /// breaks, and retain every selected content cell.
    fn retained_selection_text(&self, range: ((usize, usize), (usize, usize))) -> Option<String>;

    /// Return copy-mode navigation geometry for one retained row. Trailing
    /// unused cells are omitted, while wide-character spacer cells remain part
    /// of the layout.
    fn retained_row_layout(&self, index: usize) -> Option<RetainedRowLayout>;

    /// Jump the viewport so the row `offset` lines above the live bottom sits at
    /// the top (clamped to `history_len()`); `0` is live. Lands on a search match
    /// (docs/63). No-op on the alternate screen, like `scroll`.
    fn scroll_to(&mut self, offset: usize);

    /// Whether the child is on the **alternate screen** (a full-screen app like
    /// vim/less/a TUI agent). The alt screen has no scrollback, so callers
    /// forward wheel input to the app instead of scrolling a history buffer.
    fn alt_screen(&self) -> bool;

    /// Whether the child requested **mouse reporting** (any tracking mode). When
    /// true the app owns the mouse — including the wheel — so callers forward
    /// wheel/click events to it as escape sequences (e.g. a TUI agent scrolling
    /// its own transcript) rather than scrolling luvus's scrollback.
    fn mouse_report(&self) -> bool;

    /// Whether the pane asked for alternate scrolling on the alternate screen.
    /// It receives arrow-key scroll input instead of host history movement.
    fn alternate_scroll(&self) -> bool;

    /// Whether the child enabled application cursor mode. Combined with paste
    /// and mouse modes, this lets the input layer leave pager keys alone.
    fn application_cursor(&self) -> bool;

    /// Whether the child requested unambiguous CSI-u encoding for control keys.
    /// Input encoding must honor this for chords whose legacy byte loses the
    /// original key identity, such as Ctrl+/ versus Ctrl+7.
    fn disambiguate_escape_codes(&self) -> bool;

    /// Whether the child also requested **drag/motion tracking** (1002/1003) —
    /// press-and-move events are forwarded only then, so a click-only (1000)
    /// app isn't spammed with motion it never asked for.
    fn mouse_drag(&self) -> bool;

    /// Whether the child requested **any-motion tracking** (1003) — hover
    /// movement with no button held is reported only then.
    fn mouse_motion(&self) -> bool;

    /// Whether mouse reports should use the modern **SGR** (1006) encoding
    /// rather than the legacy X10 byte encoding.
    fn sgr_mouse(&self) -> bool;

    /// Whether the child enabled **bracketed paste** (DECSET 2004). When true a
    /// paste forwarded into the pane must be wrapped in `ESC[200~`/`ESC[201~`,
    /// or the program cannot tell pasted text from typed text — which is how a
    /// dropped file path reaches an agent CLI as literal characters instead of
    /// an attachment, and how vim auto-indents pasted code into a staircase.
    fn bracketed_paste(&self) -> bool;

    /// Update the effective pane appearance. Engines that implement DEC mode
    /// 2031 notify subscribed children from inside this interface.
    fn set_appearance(&mut self, _appearance: PaneAppearance) {}

    /// Dump the visible screen as ANSI so it can be replayed into a fresh
    /// engine on restore (session persistence). Trailing blanks are trimmed.
    fn snapshot_ansi(&self) -> String;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn factory_builds_a_working_default_engine() {
        let (tx, _rx) = mpsc::channel();
        let engine = create_engine(
            VtEngineKind::default(),
            20,
            3,
            tx,
            64 * 1024,
            PaneAppearance::default(),
        );
        let mut engine = engine.lock().expect("engine lock");
        engine.advance(b"hi");
        assert_eq!(engine.visible_rows()[0].trim_end(), "hi");
        assert_eq!(engine.cursor().x, 2);
    }
}
