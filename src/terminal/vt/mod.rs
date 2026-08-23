//! The terminal-emulator abstraction. The rest of luvus only ever talks to
//! `VtEngine`; the concrete implementation (`alacritty_terminal`) lives behind
//! it so it can be swapped (e.g. to `termwiz` for inline images) without
//! touching the app. See docs/05-pty-and-terminal.md.

pub mod alacritty;

use ratatui::style::{Color, Modifier};

/// A rendered cell's style, already mapped to ratatui colors/modifiers so the
/// trait surface stays free of engine-specific types. The cell's *symbol* (its
/// grapheme cluster) is passed alongside as a `&str`, not stored here, so the
/// common one-char case needs no per-cell allocation.
pub struct RenderCell {
    pub fg: Color,
    pub bg: Color,
    pub mods: Modifier,
}

#[derive(Clone, Copy)]
pub struct Cursor {
    pub x: u16,
    pub y: u16,
    pub visible: bool,
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
    pub exact_bytes: bool,
}

/// Minimal terminal-emulator surface. Owns the grid + scrollback.
pub trait VtEngine: Send {
    /// Feed child output. Must never panic on arbitrary bytes.
    fn advance(&mut self, bytes: &[u8]);

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

    /// Bottom `n` rows of the visible grid, for agent detection. Independent of
    /// the user's scroll position.
    fn detection_text(&self, n: u16) -> String;

    /// Every visible row as a plain string (one char per cell, full width,
    /// untrimmed) — used to copy a mouse text selection.
    fn visible_rows(&self) -> Vec<String>;

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
    fn retained_row_text(&self, index: usize) -> Option<String>;

    /// Visit retained rows oldest-first using one reusable line buffer. The
    /// callback must not retain the borrowed text after it returns.
    fn for_each_retained_row(&self, f: &mut dyn FnMut(usize, &str));

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

    /// Dump the visible screen as ANSI so it can be replayed into a fresh
    /// engine on restore (session persistence). Trailing blanks are trimmed.
    fn snapshot_ansi(&self) -> String;
}
